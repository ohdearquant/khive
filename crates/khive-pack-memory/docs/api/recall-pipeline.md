# Recall Pipeline

`memory.recall` combines FTS5 and model-specific vector retrieval, fuses and hydrates candidates, applies memory scoring, and returns namespace-safe results. This reference follows the request from handler entry through response assembly.

## Request parsing and namespace scope

`RecallParams` rejects unknown fields. The required `query` is accompanied by optional limits, memory type, score and salience floors, configuration, fusion strategy, embedding model, score breakdowns, tags, entity names, full-content selection, serving profile, and exact namespace override.

An absent namespace uses the dispatch token's visible namespace set and remains byte-identical to the legacy path. An explicit namespace is parsed with `Namespace::parse` and restricts FTS, vector loading, ANN post-filtering, and over-fetch to that exact namespace. Dispatch normally pre-applies this escape to the token; handler-side parsing is defense in depth for direct callers. Invalid namespaces are per-operation errors and are never coerced.

Tag matching reads `properties.tags`; `any` is OR and `all` is AND. A missing or non-array tag property does not match.

## End-to-end deadline

`MemoryPack::handle_recall_with_deadline` wraps the entire pipeline: profile resolution, FTS and vector gathering, ANN widening, hydration, fusion, scoring, MMR, and supersedes suppression. The default is 30 seconds through `KHIVE_MEMORY_RECALL_DEADLINE_MS`.

`params.config.recall_deadline_ms` overrides the process value. A present override must be a positive integer; zero, negative, and malformed request values return `InvalidInput`. An absent or null value falls through. Invalid operator environment values instead log a warning and fall back to 30 seconds so one bad deployment variable cannot break every recall.

This deadline differs from `ann_ready_timeout_ms`: the former returns a typed `DeadlineExceeded` for the whole operation, while the latter degrades one vector leg to FTS-only. Timing out the outer future bounds the caller's wait but does not claim to cancel storage work already owned by the runtime.

## Query validation and routing

Noise-only queries are rejected before expensive embedding work. `is_meaningful_query` requires alphabetic or CJK content, rejects empty/symbol-only input, rejects a lone non-CJK meaningful character, and filters repeated-character gibberish.

FTS CJK bypass uses `contains_cjk`, while dense multilingual routing uses the broader `needs_multilingual` signal. If multilingual routing is requested but no multilingual model is registered, recall falls back to the registered model set rather than returning no results.

`embed_query_model` checks the pack-local LRU by `(model, query)` and embeds on the blocking pool when absent. It uses the runtime's query-side instruction prefix, which preserves the trained retrieval space for instruction-tuned models such as multilingual-e5. Cache hits clone the stored vector result.

## FTS candidate collection

`recall_text_terms` returns sanitized query terms with a fanout cap of ten. `collect_recall_text_hits` delegates detailed term selection and gather mode to the helpers described in `crates/khive-pack-memory/docs/api/text-retrieval.md`.

All query terms pass through the storage FTS5 sanitizer. This is a correctness boundary, not cosmetic escaping: reserved operators, punctuation, `NOT`, `OR`, `@`, and quote fragments must not reach FTS5 as raw syntax. If sanitization leaves no usable term, the text leg contributes no candidates instead of issuing invalid SQL.

FTS uses the unified `fts_notes` table and filters `kind = "memory"` plus visible namespaces in the same search request. A diagnostic path may request a bounded snippet; normal recall can omit snippet generation to avoid paying for content that will be hydrated later.

## Vector candidate collection

Every selected model is embedded concurrently. The vector leg prefers the global ANN graph and uses sqlite-vec as an exact fallback when ANN cannot serve.

On a current warm graph, search begins with an over-fetch window. ANN results cover all namespaces, so IDs are hydrated and post-filtered to the token's visible set before returning. The initial window is `max(candidate_limit * 4, 32)` and is capped by corpus size.

When too few visible candidates survive, the loop may double the window for a configured number of rounds. Widening only runs when the installed graph contains namespaces outside the caller's visible set; otherwise a larger search cannot recover namespace-filtered candidates. The loop also stops at corpus exhaustion. `ann_overfetch_max_rounds = 1` disables widening.

A cold or stale graph enters `ensure_ann_for_model` under a bounded `ann_ready_timeout_ms` (default 8 seconds). A snapshot restore normally fits inside that window; a from-scratch production rebuild has exceeded 300 seconds. If another caller holds the model warm lock, or this recall begins the build itself and misses the bound, the model contributes zero vector hits, sets `ann_degraded`, and lets FTS continue. The build remains tracked and may finish for later requests. An ANN error likewise degrades rather than panicking.

When no installed graph is available after the readiness attempt, sqlite-vec performs an exact query with namespace predicates in SQL. ANN post-filtering is repeated after hydration as defense in depth.

## Fusion

Retrieval sources are labeled `text`, `vector`, or `both`. Per-model vector lists are unioned by UUID before cross-source fusion, retaining the best vector score for duplicates.

Supported strategies are:

| Request value | Behavior |
| --- | --- |
| `rrf` | Reciprocal-rank fusion with `k = 60`. |
| `weighted` | Weighted text/vector fusion; configured pack weights govern values. |
| `union` | Union candidate sets. |
| `vector_only` | Ignore text candidates. |
| `keyword_only` | Ignore vector candidates. |

Unknown strategy names return `InvalidInput`. Candidate count defaults to `max(limit * candidate_multiplier, 40)` unless `candidate_limit` is explicit.

## Serving-profile projection

Profile resolution happens before candidate scoring. An explicit `profile_id` wins; otherwise recall resolves an actor-plus-namespace binding for consumer kind `recall`. An explicit unknown ID is an error. An absent or malformed optional profile snapshot degrades to configured defaults rather than failing recall.

The response is stamped with `served_by_profile_id` only when a profile actually served it. The resolved `BalancedRecallState` projects posterior means into request-local recall weights. The configured pack state is not mutated by serving another profile.

Per-entity posterior lookup is bounded to a 10,000-entry reconstructed LRU. The entity term is neutral for candidates without a learned posterior and is clamped to a ±15% multiplier; details are in `crates/khive-pack-memory/docs/api/scoring.md`.

## Entity-anchored candidates

When callers supply `entity_names`, those explicit values drive the entity match adjustment. Otherwise recall derives lookup candidates from the query, performs one case-insensitive batched entity-name lookup, and retains only strings naming real KG entities. This gives lowercase and unsegmented CJK queries a precision-preserving entity path without turning ordinary lexical overlap into a second relevance score.

Entity-anchored notes are loaded in one batch and merged with retrieval candidates. Candidate extraction and quota rules are documented in `crates/khive-pack-memory/docs/api/scoring.md`.

## Hydration, scoring, and response

Candidate UUIDs are hydrated in batches. Deleted notes, non-memory notes, disallowed namespaces, memory-type mismatches, tag mismatches, and values below raw-salience or final-score floors are removed.

Recall resolves legacy missing properties at read time: memory type defaults to episodic, and salience and decay factor use the same type-specific defaults as `memory.remember`. Age never goes below zero. Fusion relevance, decayed salience, independent temporal recency, profile weight projection, and the per-entity posterior term feed the final score.

Results sort deterministically by descending score with stable tie behavior. Optional score breakdowns expose component contributions and profile effects. `full_content = false` truncates returned content to 200 characters; ranking always uses full stored content.

Superseded candidates are suppressed by inbound `supersedes` graph edges, with the archive-compatible property shortcut as a secondary route. MMR reduces near-duplicate prefixes, and the token budget bounds aggregate response text.

## Subhandlers

The dotted subhandlers expose individual pipeline stages for composition and diagnostics:

- `memory.recall_embed`: embedding model and dimensions, with vectors omitted unless requested.
- `memory.recall_candidates`: raw source candidates.
- `memory.recall_fuse`: fused candidates before final scoring.
- `memory.recall_rerank`: configured feature reranking.
- `memory.recall_score`: score and component breakdown for one candidate.

They are `Visibility::Subhandler`, so they do not appear in the public verb catalog.
