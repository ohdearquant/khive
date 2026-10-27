# Recall Pipeline

`memory.recall` combines FTS5 and model-specific vector retrieval, fuses and hydrates candidates, applies memory scoring, and returns namespace-safe results. This reference follows the request from handler entry through response assembly.

## Request parsing and namespace scope

`RecallParams` rejects unknown fields. The required `query` is accompanied by optional limits, memory type, score and salience floors, configuration, fusion strategy, embedding model, score breakdowns, tags, entity names, full-content selection, serving profile, and exact namespace override.

`created_after` and `created_before` bound results to a half-open creation-time window (`created_at >= after`, `created_at < before`), the `brain.event_counts` since/until convention. Each bound must be a full RFC 3339 timestamp with offset; date-only and offset-less forms are rejected rather than anchored, and a window where `created_after` is not earlier than `created_before` is rejected as empty.

An absent namespace uses the dispatch token's visible namespace set and remains byte-identical to the legacy path. An explicit namespace is parsed with `Namespace::parse` and restricts FTS, vector loading, ANN post-filtering, and over-fetch to that exact namespace. Dispatch normally pre-applies this escape to the token; handler-side parsing is defense in depth for direct callers. Invalid namespaces are per-operation errors and are never coerced.

Tag matching reads `properties.tags`; `any` is OR and `all` is AND. A missing or non-array tag property does not match.

## End-to-end deadline

`MemoryPack::handle_recall_with_deadline` wraps the entire pipeline: profile resolution, FTS and vector gathering, ANN widening, hydration, fusion, scoring, MMR, and supersedes suppression. The default is 30 seconds through `KHIVE_MEMORY_RECALL_DEADLINE_MS`.

`params.config.recall_deadline_ms` overrides the process value. A present override must be a positive integer; zero, negative, and malformed request values return `InvalidInput`. An absent or null value falls through. Invalid operator environment values instead log a warning and fall back to 30 seconds so one bad deployment variable cannot break every recall.

This deadline differs from `ann_ready_timeout_ms`: the former returns a typed `DeadlineExceeded` for the whole operation, while the latter degrades one vector leg to FTS-only. Timing out the outer future bounds the caller's wait but does not claim to cancel storage work already owned by the runtime.

## Query validation and routing

Noise-only queries are rejected before expensive embedding work. `is_meaningful_recall_query` requires alphabetic or CJK content, rejects empty/symbol-only input, rejects a lone non-CJK meaningful character, and filters repeated-character gibberish. The deprecated `is_meaningful_query` function delegates to this unchanged recall policy for one release; it is distinct from the text crate's `is_meaningful_text_query` policy.

`contains_cjk` (more than 15% CJK characters) sets the FTS CJK bypass, which skips document-frequency term selection in FTS gather mode and sends the first `term_k` query terms, in query order, to the ranked FTS search. The flag is read only when FTS gather is enabled, which it is not by default, and it never removes the FTS leg. The dense/vector leg has no separate query-language routing: it always searches every configured embedding engine (issue #1115), whether or not the query contains CJK. The query's script matters at fusion; see "Script-aware model weighting" below.

`embed_query_model` checks the pack-local LRU by `(model, query)` and awaits query embedding on a miss. Dropping the request cancels a built-in model's query embedding only while its job is still queued for the embedding worker; once the worker has started it, inference runs to completion and its result is discarded. A pack-registered provider's embedding runs inline and is dropped with the request. Shared cold model initialization and its initialization event continue independently. It uses the runtime's query-side instruction prefix, which preserves the trained retrieval space for instruction-tuned models such as multilingual-e5. Cache hits clone the stored vector result.

## FTS candidate collection

`recall_text_terms` returns sanitized query terms with a fanout cap of ten. `collect_recall_text_hits` delegates detailed term selection and gather mode to the helpers described in `crates/khive-pack-memory/docs/api/text-retrieval.md`.

All query terms pass through the storage FTS5 sanitizer. This is a correctness boundary, not cosmetic escaping: reserved operators, punctuation, `NOT`, `OR`, `@`, and quote fragments must not reach FTS5 as raw syntax. If sanitization leaves no usable term, the text leg contributes no candidates instead of issuing invalid SQL.

FTS uses the unified `fts_notes` table and filters `kind = "memory"` plus visible namespaces in the same search request. A diagnostic path may request a bounded snippet; normal recall can omit snippet generation to avoid paying for content that will be hydrated later.

## Vector candidate collection

Every selected model is embedded concurrently. The vector leg prefers the global ANN graph and uses sqlite-vec as an exact fallback when ANN cannot serve.

Per-model vector collection also runs concurrently. Each model independently checks its durable epoch, searches a warm cache, may wait for detached readiness work, and may fall back to an exact scan. Running these independent I/O paths concurrently avoids paying their latency serially, using the same single-model versus multiple-model dispatch pattern as query embedding. A retrieval failure degrades the affected model rather than aborting recall across every engine (ADR-031 engine-failure isolation).

On a current warm graph, search begins with an over-fetch window. ANN results cover all namespaces, so IDs are hydrated and post-filtered to the token's visible set before returning. The initial window is `max(candidate_limit * 4, candidate_limit + 32)` and is capped by corpus size.

When too few visible candidates survive, the loop doubles the fetch window per retry for a configured number of rounds, bounded by corpus exhaustion rather than a candidate cap. Widening only runs when the installed graph contains namespaces outside the caller's visible set; otherwise a larger search cannot recover namespace-filtered candidates. The loop also stops at corpus exhaustion. `ann_overfetch_max_rounds = 1` disables widening.

A cold or stale graph enters `ensure_ann_for_model` under a bounded `ann_ready_timeout_ms` (default 8 seconds). A snapshot restore normally fits inside that window; a from-scratch production rebuild has exceeded 300 seconds. If another caller holds the model warm lock, or this recall begins the build itself and misses the bound, the model contributes zero vector hits, sets `ann_degraded`, and lets FTS continue. The build remains tracked and may finish for later requests. An ANN error likewise degrades rather than panicking.

When no installed graph is available after the readiness attempt — except when `ann_ready_timeout_ms` expired, where the model deliberately contributes no vector hits rather than paying an O(corpus) exact scan — sqlite-vec performs an exact query with namespace predicates in SQL. ANN post-filtering is repeated after hydration as defense in depth.

## Fusion

Retrieval sources are labeled `text`, `vector`, or `both`. How per-model vector lists reach fusion depends on the configured strategy:

- `rrf` and `union` keep each embedding model's vector hits as a separate source, in `vector_hits_per_model` order, followed by the text source. A note ranking in more than one model's results gets one rank contribution per model instead of being collapsed to its best raw score.
- `weighted` first unions all per-model vector hits into one combined vector source by UUID (retaining the best score per duplicate), then fuses `[combined_vector, text]` under the configured vector/keyword weights.
- `vector_only` and `keyword_only` union per-model vector hits the same way as `weighted` and route only the relevant arm; the unused arm is passed as an explicit empty source so the two-arm `[vector, keyword]` positions stay stable rather than being rebound.

A strategy that discards an arm never runs it. `keyword_only` does not embed the query and does not enter ANN, the fresh-tail leg, or sqlite-vec, so it never degrades; `vector_only` does not run FTS. Under a session fence (`consistency` or a `visibility_token`), a `keyword_only` recall still waits for the fence's ANN watermark before it searches. The handler-level widening loop re-runs only the kept arm and judges corpus exhaustion on that arm alone, so a `keyword_only` query whose text arm comes back short stops after one round however many vector rows the corpus holds. `rrf`, `weighted`, and `union` run both arms as before. The diagnostic subhandlers always run both arms.

Supported strategies are:

| Request value  | Behavior                                                            |
| -------------- | ------------------------------------------------------------------- |
| `rrf`          | Reciprocal-rank fusion with the configured `k`, or `60` if none.    |
| `weighted`     | Weighted text/vector fusion; configured pack weights govern values. |
| `union`        | Union candidate sets.                                               |
| `vector_only`  | Ignore text candidates.                                             |
| `keyword_only` | Ignore vector candidates.                                           |

Unknown strategy names return `InvalidInput`. Candidate count defaults to `max(limit * candidate_multiplier, 40)` unless `candidate_limit` is explicit.

### Script-aware model weighting

A query is CJK-dominant when its CJK characters (`is_cjk_char`) outnumber its other letters. Digits, whitespace, and punctuation count for neither side, and a tie is not dominant, so `世界 hi` is not dominant while `世界你好 hi` is.

For a CJK-dominant query, recall fuses no candidates from an English-only embedding model (`ENGLISH_ONLY_EMBEDDING_MODELS` in `config.rs`, currently `all-minilm-l6-v2`). The model is searched as usual, but its list enters fusion empty, so its weight is zero under `rrf`, `weighted`, `union`, `vector_only`, and weighted-RRF configurations. The empty list keeps its source slot, so the per-source weights of a weighted strategy stay aligned. An English WordPiece vocabulary maps most CJK characters to the unknown token and scores unrelated CJK texts as near-duplicates, so its candidates otherwise outrank correct hits from a model that can read the query.

Three conditions keep the list:

- No other embedding model contributed a candidate. A store whose only model is English-only keeps its vector leg.
- The request names `embedding_model`. An explicit model choice is never overridden.
- The query is not CJK-dominant. English and mixed queries fuse exactly as before.

The rule changes fusion only. A result's reported raw `score` still takes the best similarity over every model searched, and the per-model candidate diagnostics still list each model's own hits.

## Serving-profile projection

Profile resolution happens before candidate scoring. An explicit `profile_id` wins; otherwise recall resolves an actor-plus-namespace binding for consumer kind `recall`. An explicit unknown ID is an error. An absent or malformed optional profile snapshot degrades to configured defaults rather than failing recall.

Every result carries `serve_attribution`: `profile` with `served_by_profile_id` when a readable profile actually served it, `unattributed` when a selected profile record could not be read, or `unspecified` when no profile resolved. The latter two omit the profile id; only `unspecified` permits later binding/default fallback. The resolved `BalancedRecallState` projects posterior means into request-local recall weights. The configured pack state is not mutated by serving another profile.

Per-entity posterior lookup is bounded to a 10,000-entry reconstructed LRU. The entity term is neutral for candidates without a learned posterior and is clamped to a ±15% multiplier; details are in `crates/khive-pack-memory/docs/api/scoring.md`.

## Entity-anchored candidates

When callers supply `entity_names`, those explicit values drive the entity match adjustment. Otherwise recall derives lookup candidates from the query, performs one case-insensitive batched entity-name lookup, and retains only strings naming real KG entities. This gives lowercase and unsegmented CJK queries a precision-preserving entity path without turning ordinary lexical overlap into a second relevance score.

Entity-anchored notes are loaded in one batch and merged with retrieval candidates. Candidate extraction and quota rules are documented in `crates/khive-pack-memory/docs/api/scoring.md`.

## Hydration, scoring, and response

Candidate UUIDs are hydrated in batches. Deleted notes, non-memory notes, disallowed namespaces, memory-type mismatches, tag mismatches, creation instants outside the requested `created_after`/`created_before` window, and values below raw-salience or final-score floors are removed. The handler-level re-gather loop counts only window-eligible candidates when deciding whether to widen, so strong out-of-window rows cannot satisfy the stop condition and starve eligible rows deeper in the corpus.

Recall resolves legacy missing properties at read time: memory type defaults to episodic, and salience and decay factor use the same type-specific defaults as `memory.remember`. Age never goes below zero. Fusion relevance, decayed salience, independent temporal recency, profile weight projection, and the per-entity posterior term feed the final score.

Results sort deterministically by descending score with stable tie behavior. Optional score breakdowns expose component contributions and profile effects. `full_content = false` truncates returned content to 200 characters; ranking always uses full stored content.

Superseded candidates are suppressed by inbound `supersedes` graph edges, with the archive-compatible property shortcut as a secondary route. The inbound-edge lookup walks stable storage pages to exhaustion, so multiple superseders of one candidate cannot hide an edge targeting another candidate. MMR reduces near-duplicate prefixes, and the token budget bounds aggregate response text. Both token-budget factors must be positive and their effective character-budget product must be representable; invalid configurations fail before retrieval. Default responses are arrays; when the budget removes candidates, surviving results carry `truncated: true`. When the budget removes every ranked candidate, the default response becomes `{ "results": [], "truncated": true }` so the cutoff stays distinguishable from a genuine no-match, which remains an unadorned empty array.

## Slow-request stage telemetry

Every completed recall taking at least ten seconds emits the unconditional
`memory.recall exceeded slow-request threshold` warning. Alongside total and
result-shape metadata it always names `embed_ms`, `fts_ms`, `ann_ms`,
`fresh_tail_ms`, and `hydrate_ms`, each paired with a boolean `*_attempted`;
no profiling environment flag is required. A false flag means the stage was not
entered, and its milliseconds field remains zero for compatibility. A true flag
with zero milliseconds means the stage ran in under one millisecond (including
an immediate return); it does not imply that the stage found candidates or
succeeded without degradation. No-model recall skips embedding and ANN; the exact
sqlite-vec route is included in ANN time and skips fresh-tail. If any concurrent
model or sequential widening round enters a stage, its attempted flag is true.
The fields are wall-time attribution, not an additive critical path: FTS and
the vector arm run concurrently, and concurrent embedding models contribute
the maximum ANN and fresh-tail duration. Sequential handler-level widening
rounds and their hydration passes are accumulated. The opt-in
`KHIVE_RECALL_PROFILE` stream remains available for higher-volume local
profiling and is not required for this production warning.

The ten-second threshold follows the `knowledge.compose` precedent (#887 / PR #915). The selection rationale placed it above the expected subsecond common recall path and inside the default thirty-second recall deadline, so slow but completing requests would become visible in the daemon log before a deadline or client timeout.

## Per-call stage timings

`include_timings = true` returns the same stage attribution for one call, in
microseconds, without any environment variable. The response gains a `timings`
object: an array response becomes `{results, timings}` and an object response
(truncated, degraded, or verbose multi-model) keeps its fields and gains
`timings`. The object carries `total_us` (handler time up to the response),
`embed_us`, `fts_us`, `ann_us`, `fresh_tail_us`, and `hydrate_us` accumulated
over all rounds with the same concurrency rules as the slow-request warning,
`retrieval_rounds`, and `rounds[]`. Each round reports its `candidate_limit`,
`text_hits`, `vector_hits` (summed over models, as gathered before hydration),
`hydrated` (visible memory rows after hydration), `eligible` (rows the strategy
and creation window can keep, the widening stop count), and its own stage
times. A stage that did not run is `null`, never zero. Under a session fence
the round reports the attempt that produced the candidates.

## Subhandlers

The dotted subhandlers expose individual pipeline stages for composition and diagnostics:

- `memory.recall_embed`: embedding model and dimensions, with vectors omitted unless requested.
- `memory.recall_candidates`: raw source candidates.
- `memory.recall_fuse`: fused candidates before final scoring.
- `memory.recall_rerank`: configured feature reranking.
- `memory.recall_score`: score and component breakdown for one candidate.

They are `Visibility::Subhandler`, so they do not appear in the public verb catalog.
