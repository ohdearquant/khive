# khive-pack-knowledge — Algorithm Notes

## TF-IDF Search (`knowledge.search`)

The search path uses a multi-signal TF-IDF variant with optional embedding rerank and optional
query decomposition.

### Scoring

For each candidate atom/domain, the score is a weighted sum of per-field TF scores multiplied
by the global IDF of each query term:

```
score = sum_terms(idf(term) * (
    w_exact_name  * exact_match(term, name)  +
    w_name        * tf(term, name)           +
    w_description * tf(term, description)   +
    w_tags        * tf(term, tags)           +
    w_content     * tf(term, content)        +
    w_bigram      * bigram_match(term, name) +
    coverage_alpha * coverage_bonus
))
```

Default weights: `w_exact_name=5.0`, `w_name=3.0`, `w_description=1.5`, `w_tags=1.25`,
`w_content=1.0`, `w_bigram=2.0`, `expand_discount=0.35`, `coverage_alpha=0.5`.

### Query Decomposition

When `decompose=true` and the query has >= `decompose_threshold` (default 4) non-stop terms,
the query is split into sub-queries. Each sub-query scores independently; candidates that
appear in multiple sub-query results receive an `intersection_bonus` (default 0.25) multiplier.

### Embedding Rerank

When `rerank=true` (default) and an embedder is configured, the top candidates from TF-IDF
are reranked by cosine similarity between the query embedding and atom content embeddings:

```
final_score = rerank_alpha * norm_tfidf + (1 - rerank_alpha) * cosine_sim
```

Default `rerank_alpha=0.7` (TF-IDF dominant). The TF-IDF score is normalized to [0, 1] by
dividing by the maximum TF-IDF score in the candidate set before blending.

### Vamana ANN Signal

In parallel with TF-IDF, if a Vamana ANN index is warm (populated via `knowledge.index
rebuild_ann=true`), the query embedding is also used for ANN search. ANN hits are fused with
TF-IDF hits via RRF (Reciprocal Rank Fusion) with k=60.

## Fold (`knowledge.fold`)

Uses a greedy knapsack selector from `khive-fold`. Candidates are sorted by
`score * category_weight * epistemic_weight` and selected greedily until the token budget
is exhausted or all candidates are processed. `diversity_bias` penalizes subsequent candidates
from the same category.

## Atlas Markdown Import (`knowledge.import`)

Atlas markdown format:

```
# Title

Optional pre-section body text.

## Section Heading

Section content...
```

The parser (`parse_atlas_md`) reads the `# Title` line as the atom name, collects text before
the first `## ` heading as the atom body, and maps each `## ` heading to a `SectionType` via
`SectionType::from_str_loose` (which accepts common heading aliases). Headings that don't match
any canonical type are classified as `Other`.

An optional `atlas_id:` front-matter line in the first 32 lines is extracted and stored in
`properties.atlas_id` and as `source_uri = atlas:<id>`.

The chunk strategy `"section"` (default) creates one atom + N sections per file.
The `"atom"` strategy creates one atom with the full content as `content` and no sections.

## Numeric Validation

All public request float fields (`min_score`, `intersection_bonus`, `rerank_alpha`,
`diversity_bias`, `epistemic_weight`, `category_weights.*`, `candidates[*].score`,
`candidates[*].information_gain`, `weights.*`) are validated with `is_finite()` at the handler
boundary before being cast to `f32`. Non-finite values return `RuntimeError::InvalidInput`.
