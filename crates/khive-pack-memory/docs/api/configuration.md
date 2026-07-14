# Recall Configuration Reference

`RecallConfig`, `RecallFtsGatherConfig`, and `ScoringConfig` define the retrieval and scoring policy for `memory.recall`. This reference groups validation, defaults, and environment fallbacks by the function that consumes them.

## `RecallConfig`

The three primary weights are relevance `0.70`, salience `0.20`, and temporal `0.10`. Validation requires finite, non-negative weights with a positive sum. Temporal half-life must be positive, per-reranker weights must be finite and non-negative, a provided candidate limit must be positive, and the score and salience floors must be finite. Nested gather/scoring configuration is not validated here.

Retrieval defaults include a candidate multiplier of 20, optional explicit candidate limit, weighted vector/text fusion, score floor zero, salience floor zero, and breakdowns disabled. Weighted fusion defaults to vector `0.7` and text `0.3`, preserving the prior semantic-over-keyword preference while respecting score magnitude. RRF remains selectable.

A tuning sweep of 116 configurations on the synthetic contract corpus produced the same recall@10 (`0.9333`) for every configuration. That corpus therefore supplied no evidence for changing the prior defaults; an embedding-enabled corpus with synonym and partial-match queries is needed before retuning.

`validate()` returns `RuntimeError::InvalidInput` for inconsistent values. `try_from_value()` deserializes JSON and validates in one operation.

## Candidate and ANN controls

`candidate_limit`, when set, caps each retrieval path before fusion. Otherwise `limit * candidate_multiplier`, with a floor of 40, preserves legacy behavior.

`ann_overfetch_max_rounds` controls namespace-aware ANN widening. Round one is the initial fetch and later rounds double the window until enough visible candidates survive or the corpus is exhausted. `Some(1)` disables widening. When absent, `ANN_OVERFETCH_MAX_ROUNDS` supplies a process-wide value, defaulting to three.

`ann_ready_timeout_ms` bounds a cold-miss call to `ensure_ann_for_model` before that model degrades to FTS-only. It falls back to `KHIVE_MEMORY_ANN_READY_TIMEOUT_MS`, default 8,000 ms. The bound exists because recall and boot warming share the same model single-flight lock; without a timeout, a recall arriving during a from-scratch warm could wait for the full build, observed above 300 seconds in issue #836.

`recall_deadline_ms` bounds the complete pipeline and defaults to 30,000 ms through `KHIVE_MEMORY_RECALL_DEADLINE_MS`. Per-request validation and operator fallback behavior are described in `crates/khive-pack-memory/docs/api/recall-pipeline.md`.

## `DecayModel`

Decay operates on raw salience and age in days:

| Variant | Formula |
| --- | --- |
| `Exponential` | `salience * exp(-decay_factor * age_days)` |
| `Hyperbolic` | `salience / (1 + decay_factor * age_days)` |
| `PowerLaw` | `salience * half_life / (half_life + age_days)` |
| `None` | raw salience |

Exponential and hyperbolic use the note's own decay factor. Power-law uses its configured half-life override. The independent temporal recency component uses `temporal_half_life_days`; it does not replace the note's salience decay.

Episodic memories default to salience `0.3` and decay factor `0.02` (about 35 days exponential half-life). Semantic memories default to `0.5` and `0.005` (about 139 days). Explicit values always win.

## `RecallFtsGatherConfig`

`from_env()` returns `None` when none of the FTS gather variables are set and an error for malformed values. Supported variables control enablement, selected-term count, selection mode, gather mode, row limit, multiplier, and CJK bypass.

`validate()` requires positive limits and multipliers and rejects inconsistent options. `effective_gather_limit(candidate_limit)` uses an explicit gather limit or saturating multiplication by the multiplier. `to_search_options(candidate_limit)` returns the storage-layer options.

Selection rules are original order, lowest document frequency, and highest IDF. Gather modes mirror the database ranked and rank-within-cap modes.

## `BrainProfileHint`

The hint names a profile, a result boost (default `1.3`), and a posterior threshold (default `0.6`). Modern serve-time profile projection resolves full profile state in the handler; the hint remains part of the configuration surface for compatibility.

## Score breakdowns

`ScoreBreakdown` reports raw relevance, raw and decayed salience, temporal recency, weighted contributions, total score, profile weight projection, and the optional per-entity posterior mean.

`profile_component` is the ratio of projected-weight score to default-weight score. It is `1.0` when no profile served the request or the default score is effectively zero. `entity_posterior_mean` is absent without a learned posterior; when present, the score has also received the bounded entity multiplier documented in `crates/khive-pack-memory/docs/api/scoring.md`.

`total()` sums the three weighted contributions. Profile and entity components describe later multiplicative effects and are not added to that sum.
