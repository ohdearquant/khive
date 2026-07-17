# Recall Reliability and Incident-Derived Guarantees

The recall implementation contains several constraints learned from production failures and concurrency regressions. This document records why those guarantees exist and how the focused tests distinguish a real fix from a coincidental pass.

## FTS parser safety: issues #388, #389, and #916

Reserved FTS5 syntax once reached SQLite through recall queries. `NOT`, `OR`, unmatched quotes, punctuation, and `@` could be parsed as operators or invalid barewords. The fix routes every query through `sanitize_fts5_query`; a fully stripped query yields an empty text leg rather than malformed SQL.

Regression coverage uses real handler dispatch and exact query/content pairs, ensuring the test exercises both sanitization and retrieval rather than a helper in isolation. `@` has a dedicated case because it previously survived the earlier sanitizer path.

## Bounded ANN readiness: issue #836

Boot warming and cold recall share a per-model single-flight lock. A recall that arrived behind a from-scratch warm could wait longer than 300 seconds. The readiness timeout now turns that model into an FTS-only contributor while leaving the tracked build alive for future calls.

Tests cover two distinct timeout shapes:

- another task deliberately holds the model lock, proving a contended recall returns within the bound;
- the recall itself starts a slow build, proving a self-build timeout also returns while the detached tracked build continues and eventually makes later recalls warm.

The uncontended path is compared byte-for-byte with the previous result, and an empty FTS fallback is allowed to return an empty result rather than fabricate a vector hit. These cases prevent a test from passing merely because FTS happened to find the seeded note.

## Stale serving and generation races: issues #750, #791, #812, and #844

Clearing the graph on every write destroyed the fast fallback. The replacement keeps an installed graph intact, marks it stale through a generation bump, and swaps in a complete newer graph atomically.

Generation tests exercise older, newer, and equal installation order. Restart tests reset the in-memory generation to zero and verify that same-cardinality corpus replacement and vector-only re-embedding are still detected by content hash. Cross-process tests simulate snapshot deletion plus durable epoch bump while a daemon retains a warm graph.

The hardest race lands a write after a background task has chosen its generation floor but before it scans. A two-way barrier guarantees this ordering; the test then verifies the stale attempt cannot win and that a chained attempt eventually installs the new generation. Guard-idle notification proves completion without timing sleeps.

## Namespace isolation and ANN over-fetch: issue #733

The ANN graph spans all namespaces. Exact namespace overrides therefore require both post-filtering and enough over-fetch to find visible candidates when unrelated namespaces occupy the nearest positions.

Coverage seeds many local filler vectors and a target in another namespace under deterministic fixed embeddings. With widening enabled the target appears; with `ann_overfetch_max_rounds = 1` it does not. A second case ensures widening is skipped when the graph's namespace set proves there are no hidden namespaces to filter. Tests retry only the warm-state precondition when asynchronous build completion is involved; they do not retry the whole assertion until it happens to pass.

The absent-namespace case remains byte-identical to legacy behavior, an explicit namespace returns only that namespace, and invalid namespace text produces a per-operation error naming the bad value.

## Brain profile scoring: ADR-104

Deterministic embedding fixtures give controlled two-dimensional similarities while kg, memory, and brain packs run together. Helpers skew profile posteriors using actual explicit feedback rather than mutating internal fields.

The suite establishes:

- two profiles can order the same candidates differently;
- the no-profile path remains unchanged;
- explicit `profile_id` overrides binding resolution and stamps the serving ID;
- profile component breakdown is neutral without projected change;
- identical store, query, and profile state produce identical output;
- candidates without entity posteriors receive exactly `1.0`;
- positive feedback lifts the target candidate;
- repeated feedback cannot escape the `[0.85, 1.15]` entity clamp;
- one profile's entity learning cannot affect another profile;
- entity posterior lookup and global weight projection remain distinct components.

Isolation fixtures keep signal counts and weights equal across profiles while targeting different notes, so a rank difference cannot be attributed to unequal global salience updates. Shared vocabulary keeps retrieval relevance close enough for the entity term to be observable.

## End-to-end deadline: issue #889

Sustained concurrent load exposed calls waiting until a 300-second client ceiling. The pack now applies a 30-second default across the whole recall future, independently of ANN readiness.

Pure parsing tests cover absent, null, positive, zero, negative, and non-numeric request overrides, plus operator environment fallbacks without mutating a process-lifetime `OnceLock`. Integration coverage holds the query-embedding stage with a test-controlled `Notify` so timeout behavior is real rather than simulated by sleeping outside the pipeline. It releases the blocked worker afterward, preventing a leaked `spawn_blocking` task from contaminating later tests.

A timed-out operation is followed by a normal operation to verify request isolation. Separate cases confirm both the generous default and a generous per-request override leave ordinary results unchanged.

## Background-task serialization

Non-empty recalls can schedule tracked feedback and ANN work after returning. Tests that inspect shared event rows or process-global background state use `serial(background_tasks)` and drain tracked tasks at boundaries. Empty recalls do not necessarily need that serialization because they do not fire the same follow-up path.

This is not simply test hygiene: without draining, an event from one registry can arrive during another test and create a false profile stamp or count. Fixed-vector providers also make cosine relationships independent of real embedding semantics.

## Mutation-hook coverage

Prune and generic KG update, delete, and merge all alter the memory vector corpus without calling `memory.remember`. Tests construct a registry with the note-mutation hook explicitly installed, warm the ANN once after all seed notes exist, perform one mutation, and assert the cached generation becomes stale.

Merge is seeded and warmed only after both notes exist. Warming after the first note and then remembering the second would already bump the generation, allowing the merge assertion to pass without the merge hook. This non-discriminating setup caught an earlier draft of the regression test.
