# Memory and Recall

The memory pack keeps durable context that should be useful beyond the current
session. A memory is a note with an importance (`salience`) and an age-based
decay rate. Recall searches memories by meaning and keywords, then weighs the
match against their decayed importance and recency.

Use it for session conclusions, handoffs, corrections, preferences, and
reusable operational knowledge. Use the knowledge graph for durable things and
their relationships: entities, documents, projects, and explicit edges. A
memory can point back to a graph record with `source_id` when the context needs
both kinds of persistence.

Complete verb signatures and the current parameter contract live in the
[memory pack handler declarations](../../crates/khive-pack-memory/src/pack.rs)
and the [API reference](api-reference.md#memory-pack--5-verbs). This page
covers the working loop and the behaviour that is easy to miss.

## The remember and recall loop

1. Store the conclusion rather than a raw transcript. State what changed, why
   it matters, and any next constraint.
2. Recall relevant context before resuming related work.
3. Give feedback after using a result so subsequent recall can tune its
   posteriors.
4. Periodically review low-value or expired memories before pruning them.

### Store a durable conclusion

```text
request(ops="memory.remember(content=\"The release checklist requires a migration note before the documentation change is merged.\", memory_type=\"semantic\", salience=0.75, tags=[\"release\", \"docs\"])")
```

`memory_type` is exactly `episodic` or `semantic`; there is no `procedural` or
`working` type. Choose `episodic` for time-bound session context and `semantic`
for knowledge expected to remain useful across many sessions.

When omitted, the defaults are type-specific:

| Type       | Salience | Decay factor | Intended lifetime             |
| ---------- | -------: | -----------: | ----------------------------- |
| `episodic` |      0.3 |         0.02 | Short-lived session context   |
| `semantic` |      0.5 |        0.005 | Durable facts and conclusions |

Higher salience makes a memory more prominent in recall. Higher decay makes it
lose importance more quickly. Treat high salience as scarce: if every memory is
important, it stops distinguishing what matters.

### Recall focused context

```text
request(ops="memory.recall(query=\"release documentation decisions\", memory_type=\"semantic\", tags=[\"release\", \"docs\"], tag_mode=\"all\", min_score=0.35, min_salience=0.6, limit=5)")
```

Recall combines full-text search and vector search. Its hybrid fusion supports
reciprocal-rank fusion (RRF), as well as configured weighted fusion, before
decay-weighted ranking. It is therefore not merely a chronological list: a
strong, recent, relevant memory can outrank an older one with similar text.

Use `min_score` to discard weak matches and `min_salience` to exclude memories
that were stored as less important. `tags` is a stored-memory filter;
`tag_mode="any"` matches one or more supplied tags, while `"all"` requires
every supplied tag. Filter by `memory_type` when you know whether you need
session context or durable knowledge.

### Credit a useful result

```text
request(ops="memory.recall(query=\"release documentation decisions\", limit=1) | brain.auto_feedback(query=\"release documentation decisions\", results=[{\"id\": $prev[0].id}])")
```

This chains the first returned memory into automatic positive feedback. For an
explicit rating or correction, use `memory.feedback` with the recalled memory
ID and the appropriate signal. Feedback updates the recall posteriors; it is a
ranking signal, not a rewrite of the memory's content.

## Maintenance

`memory.prune` soft-deletes memories selected by low salience and/or expiry.
Run it with `dry_run=true` first, inspect `would_prune`, then repeat without the
flag only when the selection is correct. `memory.vacuum` reclaims database space
after soft deletion; it does not choose memories to remove.

```text
request(ops="memory.prune(min_salience=0.2, dry_run=true)")
```

## Gotchas

- A fresh write is stored immediately, but its vector/ANN index warms
  asynchronously. It may not be semantically recallable right away; retry
  later instead of treating an initial miss as a failed write.
- Decay changes recall rank, not whether a memory exists. Use pruning when you
  want to remove stale material.
- `min_score` filters the composite recall rank; use it as a quality floor, not
  as a statement of exact semantic similarity.
- Tags only filter memories that were stored with tags. `tag_mode` has no
  effect without a non-empty `tags` filter.
- Feedback belongs to a result you actually evaluated. A correction is useful
  when the remembered claim is wrong; update or supersede the memory separately
  when its content must change.

## See also

- [Knowledge Graph Modeling](knowledge-graph.md): model durable entities and
  relationships
- [Search and Retrieval](search.md): retrieval and query behaviour
- [GTD Task Management](tasks.md): task state, which is distinct from retained
  context
- [Memory API reference](api-reference.md#memory-pack--5-verbs): full verb
  signatures and parameter details
