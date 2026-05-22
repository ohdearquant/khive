---
description: Retrieve prior memory before acting - query durable context, filter by memory type, and use results without overclaiming.
---

# Recall

Recall retrieves memory notes, not every note in the graph. Use it before answering from remembered context, resuming a project, planning work, or deciding whether something has been seen before.

## Workflow

### 1. Start with a concrete query

Use the words future memory authors were likely to store:

```
request(ops="recall(query=\"marketplace memory plugin remember recall\", limit=5)")
```

Prefer distinctive nouns over generic prompts. "KG agent task queue syntax" is better than "what happened with agents".

### 2. Narrow by memory type when useful

Use `semantic` for durable facts and preferences:

```
request(ops="recall(query=\"user preference fix specs\", memory_type=\"semantic\", limit=5)")
```

Use `episodic` for session history and prior outcomes:

```
request(ops="recall(query=\"previous marketplace sweep findings\", memory_type=\"episodic\", limit=5)")
```

### 3. Read the result shape

Recall results include memory content plus scoring metadata such as salience, decay, and final score. Treat higher-ranked hits as more relevant, not automatically true.

When a hit matters, carry forward its `note_id` in your notes or response so it can be inspected later.

### 4. Adjust thresholds only after the first pass

If the first query returns too much noise, raise the threshold:

```
request(ops="recall(query=\"memory pack source_id annotates\", min_score=0.4, limit=5)")
```

If important memories may be low-salience, keep `min_score` unset and refine the query instead.

### 5. Act on absence carefully

No recall result means no matching memory was found under the current pack, namespace, query, and thresholds. It does not prove the fact is false or that no related knowledge exists in KG notes.

For project research, follow a failed recall with KG search if the `kg` pack is available:

```
request(ops="search(kind=\"note\", query=\"<topic>\", limit=10)")
```

## Patterns

### Resume a project

```
request(ops="recall(query=\"<project name> decisions blockers next steps\", limit=10)")
```

Read the hits before creating new tasks or making claims about project state.

### Check user preferences

```
request(ops="recall(query=\"user prefers\", memory_type=\"semantic\", limit=10)")
```

Use this before choosing output format, tone, or workflow when the user has previously expressed durable preferences.

### Recall by provenance keywords

If memories were tagged with source or domain words, include them:

```
request(ops="recall(query=\"ADR-036 memory recall decay ranking\", memory_type=\"semantic\", limit=5)")
```

## Anti-patterns

- **Assuming recall covers all knowledge.** It covers memory notes. Use KG search for general graph notes and entities.
- **Treating high score as proof.** A recalled memory is evidence of prior stored context, not independent verification.
- **Using broad queries first.** Broad queries bury the useful hit under generic memories.
- **Ignoring memory type.** Filtering to `semantic` or `episodic` often removes irrelevant hits.
- **Overwriting the user's current instruction with old memory.** Current explicit instruction wins over recalled context.
