# Memory and Recall

This guide covers how the memory pack works in khive: how to store memories with
appropriate salience, how decay affects recall ranking, and patterns for
effective cross-session recall.

## Two memory types

khive supports two memory types:

| Type       | What it stores                                  | When to use                                        |
| ---------- | ----------------------------------------------- | -------------------------------------------------- |
| `episodic` | Session events, conversations, task completions | Default. Context that happened at a specific time. |
| `semantic` | Patterns, insights, reusable knowledge          | Facts and rules that are useful across sessions.   |

These are the only valid values. There is no `procedural` or `working` memory
type.

## Storing memories

### Basic remember

```
request(ops="memory.remember(content=\"khive uses RRF fusion for hybrid search scoring\", salience=0.8, memory_type=\"semantic\")")
```

### Parameters

| Parameter      | Type   | Default                          | Description                                                               |
| -------------- | ------ | -------------------------------- | ------------------------------------------------------------------------- |
| `content`      | string | required                         | The memory content                                                        |
| `salience`     | float  | episodic: 0.3 / semantic: 0.5    | Importance weight for recall ranking (0.0-1.0)                            |
| `decay_factor` | float  | episodic: 0.02 / semantic: 0.005 | Higher = faster decay. 0.02 ≈ 35-day half-life; 0.005 ≈ 139-day half-life |
| `memory_type`  | string | "episodic"                       | `episodic` or `semantic`                                                  |
| `source_id`    | uuid   | none                             | Entity or note this memory annotates                                      |

### Salience calibration

Salience determines how prominently a memory surfaces during recall. Use these
ranges:

| Salience | Use for                                      | Example                                     |
| -------- | -------------------------------------------- | ------------------------------------------- |
| 0.85-1.0 | Critical directives, safety constraints      | "Never delete the production database"      |
| 0.7-0.8  | Key insights, reusable patterns, corrections | "RRF scoring requires cosine normalization" |
| 0.5-0.7  | Session summaries, routine context           | "Completed attention benchmark run"         |
| < 0.5    | Low-value, ephemeral, auto-generated         | Routine status updates                      |

A common mistake is inflating salience: if everything is 0.9+, the scoring
signal is lost and recall becomes unranked.

### Linking memories to entities

```
request(ops="memory.remember(content=\"FlashAttention-3 uses asynchronous tiling on H100\", salience=0.7, source_id=\"<entity_id>\")")
```

The `source_id` creates an `annotates` edge from the memory note to the
specified entity. This makes the memory discoverable via `neighbors` on that
entity.

## Recalling memories

### Basic recall

```
request(ops="memory.recall(query=\"attention optimization\", limit=5)")
```

Returns a scored list of matching memories:

```json
[
  {"id": "...", "content": "FlashAttention-3 uses async tiling...", "score": 0.72, "salience": 0.7, ...},
  {"id": "...", "content": "PagedAttention reduces KV cache...", "score": 0.58, "salience": 0.6, ...}
]
```

### Recall parameters

| Parameter      | Type   | Default  | Description                                |
| -------------- | ------ | -------- | ------------------------------------------ |
| `query`        | string | required | Search query                               |
| `limit`        | int    | 10       | Max results                                |
| `min_score`    | float  | none     | Minimum composite score threshold          |
| `min_salience` | float  | none     | Minimum salience filter                    |
| `memory_type`  | string | none     | Filter by memory type                      |
| `tags`         | list   | none     | Filter by tags                             |
| `tag_mode`     | string | "any"    | `any` (OR) or `all` (AND) for tag matching |

### Tag-filtered recall

```
request(ops="memory.recall(query=\"search optimization\", tags=[\"khive\", \"retrieval\"], tag_mode=\"any\")")
```

## Scoring formula

Recall ranking uses a composite score:

$$\text{composite} = 0.70 \cdot \text{retrieval} + 0.20 \cdot \text{salience} \cdot \text{decay} + 0.10 \cdot \text{temporal}$$

Where:

- **retrieval_score** (70% weight): RRF fusion of FTS5 keyword match and vector
  similarity
- **salience * decay_weight** (20% weight): the memory's importance, decayed
  over time
- **temporal_score** (10% weight): recency bonus

### Decay math

Decay follows an exponential curve:

$$w_{\text{decay}} = e^{-\lambda \cdot t}$$

where $\lambda$ is `decay_factor` and $t$ is age in days.

With the episodic default `decay_factor=0.02`:

- After 1 day: 98% of original salience
- After 7 days: 87%
- After 35 days: 50% (half-life)
- After 69 days: 25%
- After 180 days: 3%

With the semantic default `decay_factor=0.005`:

- After 1 day: 99.5% of original salience
- After 30 days: 86%
- After 139 days: 50% (half-life)
- After 365 days: 16%

Higher `decay_factor` means faster decay:

- `0.001`: very slow (693-day half-life), for permanent reference memories
- `0.005`: slow (139-day half-life), semantic default, good for durable facts
- `0.02`: moderate (35-day half-life), episodic default, good for session context
- `0.05`: fast (14-day half-life), for session-specific context
- `0.1`: very fast (7-day half-life), for truly ephemeral context

## Brain integration

The Brain pack provides Bayesian profile tuning based on feedback signals. After
recalling memories, you can feed back which results were useful:

### Auto-feedback (recommended)

```
request(ops="brain.auto_feedback(results=[{\"id\": \"<mem1_uuid>\", \"used\": true}, {\"id\": \"<mem2_uuid>\", \"used\": false}])")
```

Call this after `memory.recall` to automatically signal which results you
actually used. The brain profile adjusts its tuning over time.

### Manual feedback

```
request(ops="brain.feedback(target_id=\"<full_uuid>\", signal=\"useful\")")
```

Signals: `useful`, `not_useful`, `wrong`, `explicit_positive`,
`explicit_negative`, `correction`.

Note: `target_id` must be a full UUID (not a short prefix).

## Usage patterns

### Session summary

At the end of a work session, store key findings:

```
request(ops="memory.remember(content=\"SESSION: Completed FlashAttention-3 benchmark. Key finding: 2.3x speedup over FA2 on H100, but no improvement on A100 due to async tile dependency.\", salience=0.65, memory_type=\"episodic\")")
```

### Key insight

When you discover something reusable:

```
request(ops="memory.remember(content=\"INSIGHT: knowledge.search with rerank=true gives normalized 0-1 scores vs raw RRF ~0.016. Always use rerank for score comparison.\", salience=0.75, memory_type=\"semantic\")")
```

### Session start recall

At the beginning of a session, recall recent context:

```
request(ops="memory.recall(query=\"recent session work progress\", limit=5, memory_type=\"episodic\")")
```

Then make targeted recalls based on what you are about to work on:

```
request(ops="memory.recall(query=\"FlashAttention benchmark results\", limit=5)")
```

### Agent handoff

When handing off work to another agent:

```
request(ops="memory.remember(content=\"HANDOFF: Attention benchmark suite is ready at benchmarks/attention/. Next step: run on H100 cluster. Contact: lambda:platform for GPU allocation.\", salience=0.8, memory_type=\"episodic\")")
```

## See also

- [Search and Retrieval](search.md): how hybrid search and RRF fusion work
- [Prompt Cookbook](prompt-cookbook.md): memory verb patterns
- [GTD Task Management](tasks.md): task lifecycle (often paired with memory
  for context)
