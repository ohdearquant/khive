---
description: Store and retrieve durable agent memory — remember decisions, preferences, and session outcomes with calibrated salience; recall prior context before acting or resuming a project. Use whenever you store a memory, search memory, retrieve prior context, or check what was learned in past sessions.
---

# Store and recall memory

khive memory is how context survives across sessions. The surface is three verbs —
`memory.remember`, `memory.recall`, and `memory.feedback` — but the thing worth
learning is the _salience discipline and store-before-recall cycle_, not the verbs.
Per-verb param detail is one call away: `request(ops="memory.remember(help=true)")`.

## The pattern

### 1. Decide what belongs in memory

Memory is for context that would be expensive or unreliable to rediscover. Good
candidates: project decisions and rationale, user preferences, stable domain facts,
gotchas and failure modes, session outcomes future work should build on.

Skip memory for: one-off reasoning, raw logs, tasks (use `gtd.assign`), and
entity relationships that should be structured KG edges (use `create` + `link`).

### 2. Choose `memory_type`

`episodic` (default) for event-bound or session-tied observations. `semantic` for
durable facts, rules, or reusable knowledge. These are the **only two valid values**.
When uncertain, use `episodic` — it preserves the context of when the memory was formed.

### 3. Set salience honestly

Salience controls ranking at recall time. Default it DOWN.

| Salience  | For                                               |
| --------- | ------------------------------------------------- |
| 0.9+      | Maintainer directives, life-changing facts (rare) |
| 0.65-0.85 | Stable design decisions, high-value patterns      |
| 0.5-0.7   | Session outcomes, useful observations             |
| 0.3-0.5   | Routine episodic notes (default range)            |

Most mid-session insights belong at 0.5, not 0.8. Inflated salience fills the top
of every recall result with noise and defeats the ranking.

`decay_factor` controls how fast salience erodes over time. 0.01 ≈ 69-day half-life.
Leave it at the default unless you have a specific reason to accelerate decay.

### 4. Store with provenance when available

```
request(ops="memory.remember(content=\"ADR-036 defines recall as decay-aware ranked retrieval.\", memory_type=\"semantic\", salience=0.75, source_id=\"<entity-uuid>\")")
```

`source_id` writes an `annotates` edge from the memory note to the source entity or
note. Omit it if there is no obvious source.

Batch independent memories in one call:

```
request(ops="[memory.remember(content=\"Prefer copy-paste-ready fix specs.\", memory_type=\"semantic\", salience=0.8), memory.remember(content=\"Marketplace sweep done 2026-06-20.\", memory_type=\"episodic\", salience=0.5)]")
```

### 5. Recall before acting

Run recall at session start and before claiming no prior context exists.

```
request(ops="memory.recall(query=\"<project name> decisions blockers next steps\", limit=10)")
```

Use distinctive nouns matching words a memory author would have written. "KG agent
task queue syntax" finds results; "what happened with agents" does not.

Scores are decay-weighted hybrid (FTS + vector, weighted fusion `[0.7, 0.3]` by default; RRF
is available via `fusion_strategy="rrf"`), bounded to [0,1]. Recent notes at normal salience
score 0.10-0.25. Start without `min_score`, inspect the
returned scores, then set a threshold just below the last useful hit if needed.

Filter by type when you know what you are looking for:

```
request(ops="memory.recall(query=\"user preference output format\", memory_type=\"semantic\", limit=5)")
```

No results means no matching memory under current thresholds. It does not mean the
fact is false. Follow with a KG search if needed:

```
request(ops="search(kind=\"note\", query=\"<topic>\", limit=10)")
```

## Anti-patterns

- **Storing everything.** Routine scratch work crowds out durable context. Store the
  outcome, not the reasoning trail.
- **Inflating salience.** A memory at 0.9 should be rare. Treating 0.8 as the default
  collapses ranking.
- **Using vague content.** A future recall query depends on the specific words stored.
  Write content as if answering "what would I search for to find this?"
- **Calling `memory.recall` with a broad first query.** Broad queries bury the useful
  hit. Start specific; widen only if the specific query returns nothing.
- **Treating high recall score as proof.** A recalled memory is evidence of prior stored
  context, not independent verification. Cross-check against current state.
- **Using any `memory_type` other than `episodic` or `semantic`.** Other values are
  rejected by the handler.
