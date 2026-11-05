# Recall Configuration Reference

`RecallConfig`, `RecallFtsGatherConfig`, and `ScoringConfig` define the retrieval and scoring policy for `memory.recall`. This reference groups validation, defaults, and environment fallbacks by the function that consumes them.

## `RecallConfig`

The three primary weights are relevance `0.70`, salience `0.20`, and temporal `0.10`. Validation requires finite, non-negative weights with a positive sum. Temporal half-life must be positive, per-reranker weights must be finite and non-negative, a provided candidate limit must be positive, and the score and salience floors must be finite. Nested gather/scoring configuration is not validated here.

Retrieval defaults include a candidate multiplier of 20, optional explicit candidate limit, RRF fusion with `k = 10`, score floor zero, salience floor zero, and breakdowns disabled. Weighted fusion (vector `0.7`, text `0.3`) remains selectable per call via `fusion_strategy: "weighted"`; an explicit `fusion_strategy: "rrf"` request fuses with the configured `k` (10 by default) and uses `k = 60` only when the configured fusion is not RRF.

`validate()` returns `RuntimeError::InvalidInput` for inconsistent values. `try_from_value()` deserializes JSON and validates in one operation.

### Default selection rationale

The historical 2026-05-25 tuning sweep evaluated 116 configurations on the synthetic contract corpus and produced the same recall@10 (`0.9333`) for every configuration. That flat landscape cannot empirically distinguish the parameters; `tests/khive-contract/tune/README.md` retains the limitation and the need for a harder corpus with embeddings, synonym queries, and partial matches.

The 2026-07-21 live-corpus golden-suite sweep did distinguish the strategies. The weighted default (vector `0.7`, text `0.3`) regressed adversarial-paraphrase hit@5 from `0.471` to `0.353` and MRR from `0.321` to `0.153`, while RRF `k = 10` held the holdout suite at its best tier and was restored as the calibrated default. Min-max weighted fusion lets a keyword-only text hit outrank paraphrase hits visible only to the vector leg; rank-based RRF avoids that magnitude mismatch. Salience still contributes through the composite score weights.

## Candidate and ANN controls

`candidate_limit`, when set, caps each retrieval path before fusion. Otherwise `limit * candidate_multiplier`, with a floor of 40, preserves legacy behavior.

`ann_overfetch_max_rounds` controls namespace-aware ANN widening. Round one requests `max(candidate_limit * 4, candidate_limit + 32)` and each later round doubles the fetch window, bounded only by the round count and corpus exhaustion, until enough visible candidates survive. `Some(1)` disables widening. When absent, `ANN_OVERFETCH_MAX_ROUNDS` supplies a process-wide value, defaulting to three. A separate post-hydration re-gather loop in recall quadruples its candidate limit per pass, capped by the server-wide maximum; that loop is not governed by this setting.

`ann_ready_timeout_ms` bounds the receiver wait for a detached writable cold-miss Warm attempt, including queued admission. It falls back to `KHIVE_MEMORY_ANN_READY_TIMEOUT_MS`, default 8,000 ms. Expiry drops only the receiver; tracked admission/build/supervisor work can continue for later requests. Expiry and received not-ready outcomes select the existing degraded FTS/bounded fresh-tail path. The bound avoids waiting for the full build behind the shared model single-flight lock; see issue #836.

`recall_deadline_ms` bounds the complete pipeline and defaults to 30,000 ms through `KHIVE_MEMORY_RECALL_DEADLINE_MS`. Per-request validation and operator fallback behavior are described in `crates/khive-pack-memory/docs/api/recall-pipeline.md`.

## `DecayModel`

Decay operates on raw salience and age in days:

| Variant       | Formula                                         |
| ------------- | ----------------------------------------------- |
| `Exponential` | `salience * exp(-decay_factor * age_days)`      |
| `Hyperbolic`  | `salience / (1 + decay_factor * age_days)`      |
| `PowerLaw`    | `salience * half_life / (half_life + age_days)` |
| `None`        | raw salience                                    |

Exponential and hyperbolic use the note's own decay factor. Power-law uses its configured half-life override. The independent temporal recency component uses `temporal_half_life_days`; it does not replace the note's salience decay.

Episodic memories default to salience `0.3` and decay factor `0.02` (about 35 days exponential half-life). Semantic memories default to `0.5` and `0.005` (about 139 days). Explicit values always win.

## `RecallFtsGatherConfig`

`from_env()` returns `None` when none of the FTS gather variables are set and an error for malformed values. Supported variables control enablement, selected-term count, selection mode, gather mode, row limit, multiplier, and CJK bypass.

`validate()` requires positive limits and multipliers and rejects inconsistent options. `effective_gather_limit(candidate_limit)` uses an explicit gather limit or saturating multiplication by the multiplier. `to_search_options(candidate_limit)` returns the storage-layer options.

Selection rules are original order, lowest document frequency, and highest IDF. The gather mode is the storage layer's `TextGatherMode` (ranked, unranked, rank-within-cap); `RecallFtsGatherMode` re-exports it.

## `BrainProfileHint`

The hint names a profile, a result boost (default `1.3`), and a posterior threshold (default `0.6`). A served profile's full state is resolved in the handler, where it supplies the per-entity posteriors and the profile's `local_factor`; it does not change the scoring weights. The hint remains part of the configuration surface for compatibility.

## Score breakdowns

`ScoreBreakdown` reports the relevance the hit was ranked with, the fused score it came from, the factors of the ranking composite, raw and decayed salience, temporal recency, the additive weighted contributions, the profile weight projection, the optional per-entity posterior mean, and the terms that make up the returned `rank_score`.

`relevance` is the value the ranking composite received: the fused score after the calibrated normalization (never above 0.82), reported without further scaling. `fused_score` is the fused score before that normalization.

`composite` reports the factors of the multiplicative composite that ranks the hit, each the 32-bit value used:

```text
relevance_term = weights.relevance * relevance
temporal_boost = 1 + weights.temporal * recency
salience_boost = 1 + weights.salience * salience
adjustment_factor = net multiplier of the configured adjustments and the final clamp to [0, 1]
```

With no `reranker_weights` configured, `relevance_term * temporal_boost * salience_boost * adjustment_factor` equals `pre_entity_score`. When `reranker_weights` is configured, `pre_entity_score` is the reranker output and `composite` still describes the default composite. `adjustment_factor` is `null` when the first three factors multiply to zero: the adjustments then act on zero, and no multiplier describes the score they leave. `composite` is absent from `memory.recall_score`, which scores additively.

`temporal` and `salience_decayed` are the half-life recency and the decayed salience that the weighted reranker reads, and `weighted` splits the `RecallConfig` weights additively over `relevance`, the amplified `salience_decayed` and `temporal`. That additive composite is the one `memory.recall_score` scores with. A recall ranks by `composite`, so in a recall the three `weighted` contributions do not sum to `pre_entity_score`.

A served profile scores with the configured default weights. `weights_served` is always `"default"`. `profile_component` is a diagnostic: the ratio of the hit's score under the weights the profile's posteriors project to, to its score under the default weights. It is reported for the breakdown only and never enters `rank_score`. It is `1.0` when no profile served the request or the default score is effectively zero.

`namespace`, `pre_entity_score`, `entity_term`, `local_factor` and `duplicate_penalty` report the terms of the returned `rank_score`, each the 32-bit value used:

```text
rank_score = pre_entity_score * entity_term * local_factor - duplicate_penalty
```

`pre_entity_score` is the weighted reranker output when `reranker_weights` is configured, and the default-weight composite otherwise. `local_factor` is the serving profile's `local_factor` for a hit in the `local` namespace, and `1` for every other namespace and whenever no profile with a factor serves the request. `duplicate_penalty` is the amount the duplicate-prefix penalty subtracted, `0` when none applied; when the penalty would take the score below zero it is the whole score, and `rank_score` is `0`.

`entity_posterior_mean` reports the serving profile's per-entity Beta posterior mean for the hit's UUID. It is absent when no profile served the request or no posterior exists beyond the uninformative prior. The value also feeds the bounded rank-score multiplier `clamp(1 + 0.3 * (entity_posterior_mean - 0.5), 0.85, 1.15)` (ADR-104 §3). `None` and a mean of `0.5` are neutral; other means adjust `rank_score` by up to ±15% relative to the same candidate under a profile without a posterior for it. The scoring context is documented in `crates/khive-pack-memory/docs/api/scoring.md`.

`total()` sums the three weighted contributions, which a recall does not rank by (see above). Profile and entity components describe later multiplicative effects and are not added to that sum.
