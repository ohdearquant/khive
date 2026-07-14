# Memory Lifecycle Verbs

The memory lifecycle begins with `memory.remember`, continues through recall feedback, and ends with curation by prune and vacuum. This reference describes validation, namespace routing, mutation side effects, and failure behavior.

## `memory.remember`

`memory.remember` requires content and accepts memory type, salience, decay factor, source, tags, embedding model, and namespace. The only valid memory types are `episodic` and `semantic`; the stored `memory_type` property is always present.

Defaults differ by type:

| Type | Salience | Decay factor | Default namespace |
| --- | ---: | ---: | --- |
| episodic | `0.3` | `0.02` | caller/actor namespace |
| semantic | `0.5` | `0.005` | shared `local` namespace |

An explicit namespace overrides both routing rules. Salience must be finite and inside `[0, 1]`. Decay factor must be finite and non-negative; there is no arbitrary upper clamp. Explicit caller values always replace defaults.

The handler creates a `kind = "memory"` note and stores tags in properties. When `source_id` is supplied, it resolves the record and creates an `annotates` edge from the new memory to that source. Invalid source references fail the operation instead of creating a dangling edge.

After persistence, every affected embedding model's generation is bumped before a background warm is requested. The order ensures a rebuild cannot install as current against the pre-write floor. Since issue #791, remember does not clear the existing graph or snapshot; stale serving and atomic replacement are described in `crates/khive-pack-memory/docs/api/ann-lifecycle.md`.

## `memory.feedback`

Feedback validates its signal before routing. Accepted legacy signals are `useful`, `not_useful`, and `wrong`; semantic signals include explicit/implicit positive and negative plus correction.

Serving profile resolution has three tiers:

1. explicit configured profile;
2. actor-plus-namespace binding for consumer `recall`;
3. the memory pack's global in-process prior state.

Tier one wins over tier two. Namespace is included when dispatching to the brain pack so the registry mints the correct token. When a profile is resolved, feedback routes through `brain.feedback`; otherwise the global state updates directly. Invalid signals are rejected before either route and never poison posterior state.

## Posterior update functions

`on_recall_hit` increments total events and relevance success. Latency at or below 50,000 microseconds is a temporal success; slower hits are temporal failures. The target entity posterior receives a success.

`on_recall_miss` increments total events and records relevance and temporal failures without creating an entity posterior.

`on_explicit_feedback` applies semantic event weights when recognized, falling back to legacy signal decoding. Positive signals update salience and entity success; negative signals update failures. Correction also penalizes global relevance with its weighted strength. Unknown strings are a no-op in this low-level helper, while the public verb validates them before calling it.

The 50 ms threshold reflects normal local SQLite FTS5 latency of roughly 1–20 ms, leaves contention headroom, and stays below the 250 ms rerank budget.

## `memory.prune`

Prune selects live memory notes in the requested namespace. It can match raw salience strictly below `min_salience`, expiry at or before `before`, or both. `before` defaults to the current Unix microsecond timestamp; zero disables the expiry predicate. `dry_run` returns the count without mutation.

Deletion is soft, performed directly through `NoteStore`. That raw path bypasses the runtime mutation hooks, so the handler itself bumps ANN generations and schedules background rebuilding for affected models. A candidate disappearing after selection is tolerated as ordinary concurrent mutation.

## `memory.vacuum`

Vacuum reclaims database pages after soft deletion. It accepts no parameters and rejects unexpected fields.

SQLite `VACUUM` must run outside an open transaction. The handler issues `VACUUM;` through the writer's top-level script path (`execute_script_top_level`), which skips the usual transaction wrapper while still serializing on the single writer. Failures are returned as runtime storage errors; success reports completion without claiming how many bytes SQLite reclaimed.
