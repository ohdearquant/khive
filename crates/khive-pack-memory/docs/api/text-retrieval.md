# FTS Candidate Gathering

The text-retrieval subsystem converts recall queries into bounded FTS5 requests and optionally selects statistically useful terms. It is separate from result scoring: its job is to preserve a good candidate pool at controlled database cost.

## `recall_text_terms`

`recall_text_terms(query)` tokenizes and sanitizes query text, returning at most ten terms for the public recall path. Sanitization removes FTS5 control syntax and discards empty results. The lower-level limit form exists for tests and alternate candidate budgets.

## `RecallFtsGatherConfig`

The optimization is disabled by default, preserving ranked all-term behavior. When enabled, configuration controls maximum selected terms, selection rule, database gather mode, row cap or multiplier, and whether CJK bypasses term selection.

The effective gather limit is either the explicit value or `candidate_limit * gather_cap_multiplier`, with saturating multiplication and positive-value validation. `to_search_options` converts pack configuration into the storage-layer `TextSearchOptions` used by the database.

## `select_terms_by_stats`

`select_terms_by_stats(terms, stats, k, rule)` returns at most `k` terms and preserves deterministic tie ordering.

- `Original` takes the first `k` query terms.
- `LowestDf` prefers the smallest document frequency.
- `HighestIdf` prefers the largest Robertson-Walker inverse document frequency; with the same corpus statistics this is equivalent to lowest DF.

Missing statistics remain eligible but sort behind measured selective terms. The function returns an empty set for `k = 0` or no input terms.

## `collect_text_hits`

`collect_text_hits` receives the runtime, namespace token, query, candidate budget, snippet policy, and gather configuration. It optionally fetches term statistics, selects terms, builds the storage request, and returns FTS hits.

CJK queries can bypass statistical selection because whitespace term boundaries are not a reliable segmentation mechanism. The storage query always retains the memory kind and namespace constraints. `Ranked` returns the legacy top-ranked rows; rank-within-cap mode first bounds the match set, then ranks within that cap.

## Query embedding cache

`QueryEmbeddingCache` is a thread-safe LRU local to the pack. The key is the model name plus exact query text, so embeddings cannot cross model spaces. Capacity is non-zero and defaults to 512 entries. Reads update recency; insertion replaces an existing key and evicts the least-recently-used entry when necessary.

The cache is intentionally process-local and does not claim durability or coherence across model reconfiguration. Model identity in the key is the isolation boundary.

## Benchmark conclusion

The real-corpus FTS gather experiment found that OR match sets were dominated by near-zero-IDF English function words. Removing those terms was faster without losing candidate-pool coverage, but fixed-`k` lowest-DF/highest-IDF selection could remove meaningful terms, and per-term statistics round trips cost more than the saved gather work. Therefore `fts_gather.enabled` remains false by default.

Reproduction commands and release measurements are in `crates/khive-pack-memory/docs/benchmarks.md`.
