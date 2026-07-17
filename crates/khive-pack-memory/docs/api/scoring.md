# Scoring and Reranking

The scoring subsystem turns fused retrieval evidence, memory salience, age, profile state, and entity evidence into a deterministic score. The response `score` field is clamped to `[0, 1]`; the ordering `rank_score` is multiplied by the entity posterior term without a final clamp and may exceed `1.0`. This reference covers normalization, conditional adjustments, entity extraction, and weighted reranking.

## `calculate_score`

The archive scoring model computes:

```text
score = w_rel * relevance
      * (1 + w_temp * temporal_recency)
      * (1 + w_imp * salience)
```

Age is measured in days and clamped at zero. `decay_factor` is capped by `ScoringConfig.decay_cap` before `exp(-decay * age)` computes temporal recency. Configured `ScoreAdjustment` values then apply in order, and the final value is clamped to `[0, 1]`.

`ScoreInput` groups candidate fields to keep the function interface manageable: salience, memory type, content, creation time, decay factor, current time, normalized relevance, and entity names.

## `AdjustmentCondition` and `ScoreAdjustment`

Conditions can match memory type, bounded age, bounded salience, entity presence or absence, or the conjunction of nested conditions. Bounds are inclusive and independently optional. Entity conditions are false when no entity names are available.

Operations add, subtract, or multiply. `ScoreAdjustment::apply` leaves the score unchanged when its condition is false.

Default adjustments are:

- a `+0.05` bonus for episodic memories no older than seven days;
- a `-0.05` penalty for semantic memories at least 30 days old with salience at least `0.85`;
- a `1.3` multiplier when a query entity appears in content.

Entity matching lowercases both sides and requires character boundaries, preventing `beta` from matching `alphabet` and `car` from matching `scarcity`. Multi-word names work because only the outer boundaries matter. All-CJK strings use substring matching because CJK text does not supply equivalent alphanumeric word separators.

## Entity candidates from capitalized query terms

`extract_entity_candidates` supplies a small automatic list when callers do not pass explicit entity names. It strips punctuation, accepts capitalized tokens, removes common English function words, deduplicates case-insensitively, and caps output at `MAX_AUTO_ENTITY_NAMES` (8).

Capitalization is deliberately the only lexical signal. `EntityMatch` examines free content rather than KG references, so admitting ordinary lowercase terms would duplicate retrieval relevance and flatten top scores at the `[0, 1]` ceiling. Lowercase queries therefore produce no heuristic candidates; the real-entity lookup path below handles them precisely.

## `entity_lookup_candidates`

This broader sampler feeds a batched case-insensitive KG entity-name lookup. Recall only accepts sampled strings that name a real entity, so ordinary lowercase tokens are safe to offer.

Whitespace tokens and CJK substrings share a hard cap of `MAX_ENTITY_LOOKUP_CANDIDATES` (64). CJK substrings cover lengths 2 through 8. Each length receives a fair quota, unused quota from short runs is redistributed, and start positions are sampled evenly. A quota greater than one includes both first and final valid starts; a quota of one chooses the first. The result is fair across substring lengths and positions instead of allowing one length or one query region to dominate.

Explicit caller `entity_names` take precedence over derived candidates.

## Query language routing

`is_cjk_char` recognizes Unified CJK, Extensions A and B, compatibility ideographs, Hiragana, Katakana, and Hangul syllables. `contains_cjk` returns true when CJK exceeds 15% of all characters.

`needs_multilingual` uses a different denominator: it routes when more than 15% of alphabetic characters are non-ASCII. Punctuation, digits, and whitespace do not dilute the ratio, so `Müller`, `Müller?`, and `Müller!!!` route identically. It covers CJK, Cyrillic, Arabic, Devanagari, Hebrew, Thai, accented Latin, and other Unicode alphabetic scripts. ASCII-only non-English text is a known limitation and continues to the primary model unless a separate language detector is introduced.

## `normalize_min_score`

Finite values in `[0, 1]` pass through. Values above 1 through 100 are treated as percentages and divided by 100. NaN, infinity, negative values, and values above 100 return `MinScoreError`; the ambiguous value 1 maps to the fractional scale but equals the same normalized result on either scale.

## Relevance normalization

`normalize_rrf_scores` maps valid dual-source RRF scores into the band from `baseline_relevance` to `0.82`. The best surviving score reaches the ceiling and ties are deterministic.

`normalize_rank_fusion_scores` handles single-source raw-cosine or BM25-like values in the same calibrated band but multiplies by signal strength relative to the RRF threshold `0.025`. This prevents a weak single candidate from receiving the same relevance as a genuinely strong source result.

Both functions discard non-finite values, values below `min_rrf_relevance`, and non-positive maxima. The `0.82` ceiling reserves headroom for temporal, salience, and entity adjustments before the final clamp.

## Per-entity posterior term

`entity_posterior_term(mean, weight)` calculates:

```text
clamp(1 + weight * (mean - 0.5), 0.85, 1.15)
```

The default weight is `0.3`, making both clamp endpoints reachable at posterior means zero and one. `None` returns exactly `1.0`; an untouched memory must receive the identity multiplier rather than merely some value inside the band. The term can never move a candidate by more than ±15%.

## `weighted_rerank`

`RerankFeatures` exposes fused relevance, decay-adjusted salience, independent temporal recency, and boolean text/vector membership. Recognized weight keys are `relevance`, `salience`, `temporal`, `text_match`, and `vector_match`.

The score is `sum(weight * feature) / sum(positive weights)`. Unknown names are ignored for forward compatibility. Zero weights do not contribute. Empty, unrecognized-only, or non-positive-only maps return zero. Because of normalization, scaling every positive weight by the same factor does not change the result; a single positive feature weight returns that feature's value.

## DoS caps and MMR

`ScoringConfig::apply_dos_caps` clamps candidates to 500, token budget to 16,000, and result limit to 200. Defaults are 200 candidates, 4,000 tokens, and 10 results. MMR applies a default `0.1` penalty when the first 100 characters duplicate an earlier result.

Supersedes suppression and multilingual routing are enabled by default. A configured multilingual model is preferred; otherwise registered model names containing `multilingual` or `paraphrase` are candidates.
