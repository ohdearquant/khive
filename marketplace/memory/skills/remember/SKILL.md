---
description: Store durable agent memory with the right type, salience, decay, tags, and optional source link.
---

# Remember

Memory is for context that should survive the current session. Use it for stable facts, decisions,
preferences, recurring observations, and session outcomes that future agents should retrieve before
acting.

Do not use memory as a scratchpad. Transient todos belong in the GTD pack; structured concepts and
relationships belong in the KG pack.

## Workflow

### 1. Decide whether it belongs in memory

Store the item if it is likely to be useful later and would be expensive or unreliable to
rediscover.

Good memory candidates:

- User preferences that should shape future behavior.
- Project decisions and their rationale.
- Stable facts about a repository, workflow, or domain.
- Session outcomes that future work should build on.
- Repeated failure modes or gotchas.

Skip memory for:

- One-off intermediate reasoning.
- Raw logs or large pasted documents.
- Tasks that should be tracked with `assign`.
- Entity relationships that should be represented with `create` and `link`.

### 2. Choose a memory type

| Type       | Use when                                                                         |
| ---------- | -------------------------------------------------------------------------------- |
| `episodic` | The memory is tied to an event, session, interaction, or time-bound observation. |
| `semantic` | The memory is a durable fact, preference, rule, or reusable piece of knowledge.  |

When uncertain, use `episodic`. It is safer to preserve the context of when the memory was learned.

### 3. Store the memory

```
request(ops="memory.remember(content=\"<durable memory>\", memory_type=\"episodic\", salience=0.6)")
```

Use `salience` from `0.0` to `1.0`. Defaults are acceptable for ordinary memories; use high salience
only for context that should reliably outrank routine notes.

For stable facts:

```
request(ops="memory.remember(content=\"The memory pack stores one note kind, memory, and uses memory_type to distinguish episodic from semantic memories.\", memory_type=\"semantic\", salience=0.8, tags=[\"khive\",\"memory-pack\"])")
```

### 4. Link provenance when available

If the memory came from a source entity or note, pass its UUID:

```
request(ops="memory.remember(content=\"ADR-036 defines recall as memory-scoped retrieval with decay-aware ranking.\", memory_type=\"semantic\", salience=0.8, source_id=\"<source-uuid>\", tags=[\"adr\",\"memory\"])")
```

`source_id` creates an `annotates` relationship from the memory note to the source.

### 5. Verify retrievability

After storing important memory, check that a future query can find it:

```
request(ops="memory.recall(query=\"<distinctive phrase>\", limit=3)")
```

If recall cannot find it, rewrite the memory with clearer keywords or add tags in a new memory. Do
not create multiple near-duplicate memories unless the difference matters.

## Patterns

### Capture a session outcome

```
request(ops="memory.remember(content=\"Marketplace sweep found KG agent task queries must use tasks(...) instead of list(kind=\\\"task\\\", filter=...).\", memory_type=\"episodic\", salience=0.7, tags=[\"marketplace\",\"gtd\",\"kg-agents\"])")
```

### Store a durable preference

```
request(ops="memory.remember(content=\"User prefers copy-paste-ready fix specs with exact before and after content.\", memory_type=\"semantic\", salience=0.9, tags=[\"user-preference\",\"specs\"])")
```

### Store several independent memories

```
request(ops="[
  memory.remember(content=\"Memory pack exposes memory.remember and memory.recall verbs.\", memory_type=\"semantic\", salience=0.7),
  memory.remember(content=\"Recall should be run before claiming no prior context exists.\", memory_type=\"semantic\", salience=0.8)
]")
```

## Anti-patterns

- **Remembering everything.** Memory quality drops if routine scratch work crowds out durable
  context.
- **Using vague content.** A future `recall` query depends on specific words.
- **Inflating salience.** High-salience memories should be rare.
- **Storing tasks as memories.** Use `assign` for commitments and lifecycle tracking.
- **Storing relationships as prose only.** If the relationship matters structurally, use KG entities
  and edges.
