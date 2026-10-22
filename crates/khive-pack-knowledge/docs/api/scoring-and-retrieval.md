# khive-pack-knowledge — Algorithm Notes

## TF-IDF Search (`knowledge.search`)

The search path uses a multi-signal TF-IDF variant with optional embedding rerank and optional
query decomposition.

### Atom and domain classification

Across import, delete, listing, totals, cursor pagination, embedding coverage, and
search kind filters, a domain mirror is a row whose decoded JSON string-array
`tags` contains the exact, case-sensitive element `type:domain`. Escaped spellings
such as `type\u003adomain` count; `type:domain-extra`, `prefix:type:domain`, and
`TYPE:DOMAIN` remain ordinary atom tags. Legacy tags that are not a complete JSON
string array retain the conservative, case-sensitive raw substring check.

SQLite read predicates and Rust checks use the same classifier. Cursor indexes
cover all live rows; classification is applied before the page limit. Upgrading
rebuilds the atom cursor index without rewriting stored tags or atom content.

### Scoring

For each candidate atom/domain, the score is a weighted sum of per-field TF scores multiplied
by the global IDF of each query term:

$$
\text{score} = \sum_{t \in \text{terms}} \mathrm{idf}(t) \cdot \Bigl(
w_{\text{exact\_name}} \cdot \mathrm{exact}(t, \text{name})

- w_{\text{name}} \cdot \mathrm{tf}(t, \text{name})
- w_{\text{desc}} \cdot \mathrm{tf}(t, \text{description})
- w_{\text{tags}} \cdot \mathrm{tf}(t, \text{tags})
- w_{\text{content}} \cdot \mathrm{tf}(t, \text{content})
- w_{\text{bigram}} \cdot \mathrm{bigram}(t, \text{name})
  \Bigr) \cdot \text{coverage}^{\alpha}
  $$

Default weights: `w_exact_name=5.0`, `w_name=3.0`, `w_description=1.5`, `w_tags=1.25`,
`w_content=1.0`, `w_bigram=2.0`, `expand_discount=0.35`, `coverage_alpha=0.5`.

Coverage is the fraction of original non-stop query terms found in a candidate.
A term's own singular/plural expansion can satisfy that term; matching a different
original query term cannot. With `coverage_alpha=0.5`, a candidate covering one
of two original terms receives a factor of `sqrt(0.5)`.

### Query Decomposition

When `decompose=true` and the query has >= `decompose_threshold` (default 4) non-stop terms,
the query is split into sub-queries. Each sub-query scores independently; candidates that
appear in multiple sub-query results receive an `intersection_bonus` (default 0.25) multiplier.

### Embedding Rerank

When `rerank=true` (default) and an embedder is configured, the top candidates from TF-IDF
are reranked by cosine similarity between the query embedding and atom content embeddings:

$$
\text{final\_score} = \alpha \cdot \hat{s}_{\text{tfidf}} + (1 - \alpha) \cdot \cos(\mathbf{q}, \mathbf{d})
$$

where $\alpha$ = `rerank_alpha` (default 0.7, TF-IDF dominant) and $\hat{s}_{\text{tfidf}}$
is the TF-IDF score normalized to $[0, 1]$ by dividing by the maximum TF-IDF score in the
candidate set.

Knowledge retrieval uses the default embedder for both query and atom vectors. Accordingly,
`knowledge.index` writes only default-model atom vectors; additional registered models are not
indexed until a knowledge read path can select or fuse them.

### Learned atom weights

`knowledge.search` reads persisted `atom_weights` for candidate atom IDs in the
caller's namespace on each request, in one bounded batch. Each candidate stage fetches up to
100 candidates regardless of the requested output limit; the lexical raw candidate
pool remains unchanged. The distinct lexical/ANN union may therefore contain up
to 200 candidates. Candidates outside those stage caps are not globally reranked.
Missing weight rows apply a factor of `1.0`; stored weights use the shared
loader's `0.1` to `5.0` bounds. Domain hits always apply `1.0`.

After any embedding blend and status normalization/multipliers, the score is
multiplied by the learned factor. Search then sorts by weighted score (slug
breaks ties), applies `min_score` to that weighted score and takes the requested
limit. The resulting score can exceed `1.0`; it is not clamped. A factor of
`1.0` preserves the candidate's prior score. Each hit reports its applied factor
as numeric `atom_weight`.

A failed weight read retains the candidate scores with factor `1.0` and reports
`degraded.atom_weights_unavailable`, even when results are nonempty. The reason
is `cancelled` for request cancellation, `timeout` for a deadline or admission
timeout, and `storage` for other read failures. This fallback does not claim
that stored weight rows are absent. The read uses the original request deadline
and adds no retry. A request with no atom candidates performs no weight read.

### Vamana ANN Signal

Alongside the TF-IDF signal, if a Vamana ANN index is warm (populated via `knowledge.index
rebuild_ann=true`), the query embedding is also used for ANN search. ANN hits are fused with
TF-IDF hits via RRF (Reciprocal Rank Fusion) with $k = 60$:

$$
\mathrm{RRF}(d) = \sum_{r \in \text{rankers}} \frac{1}{k + \mathrm{rank}_r(d)}
$$

### Candidate Admission and Degradation

The FTS leg is a recall stage, not the final ranker. It queries each de-duplicated non-stop
term separately, including the scorer's singular/plural expansions, so matches separated in
the text remain eligible for TF-IDF scoring. Phase A enumerates bounded rowid windows from the
index; phase B hydrates them with namespace, deletion, status, and kind eligibility. Widening
and eligibility-scoped FTS recovery keep an ineligible prefix from hiding real matches, within
the lexical deadline. A genuine FTS miss contributes no unrelated recent rows.

`knowledge.search` returns per-hit `score_provenance` (lexical/ANN sources, successful
embedding rerank, `s_over_s_plus_1` normalization, and `calibrated: false`) plus response
`candidate_provenance`. The latter distinguishes healthy matches, filtered matches, misses,
and partial or complete lexical timeouts; its `fallback` is `ann` only when returned hits
have ANN evidence without any lexical evidence. These fields replace fixed numeric score
bands as retrieval-path evidence, not as a relevance guarantee.

Status precedence is resolved once for
candidate admission and final scoring: an explicit `exclude_status` excludes exactly that status,
so `deprecated` remains eligible unless the resolved policy excludes it; default and
`include_drafts=true` searches continue to exclude deprecated rows.

ANN-only candidates are hydrated from the canonical atom/domain tables before eligibility or RRF
admission. If deletion, status, or kind filtering consumes the initial ANN top-k, retrieval widens
the deterministic ANN prefix until the eligible target is filled or the vector corpus is
exhausted. Exhaustion is measured on the ANN prefix before fresh-tail deletes are merged, and a
live vector-store count is not used as a hard serving-bridge bound, so a newly deleted leading
candidate cannot make a full prefix look exhausted. Ineligible candidates therefore neither
consume returned slots nor distort eligible RRF ranks. Hydration queries are chunked below
SQLite's portable bind-variable ceiling. A stale id
or storage-read failure degrades the candidate pool but does not discard a valid lexical response.
Unresolved shells are never returned, and a partial response reports the number under
`degraded.hydration_failures`. The field is omitted when the count is zero. This diagnostic
composes with `knowledge.suggest`'s existing ANN-unavailable degradation object and is propagated
through auto-`knowledge.compose` when its internal suggestion is degraded.

Eligibility refill does not override caller-requested score semantics: `min_score` is reapplied
after status multipliers and learned atom weights, so a genuine score-floor rejection may still
return fewer than `limit`.

## Fold (`knowledge.fold`)

Uses a greedy knapsack selector from `khive-fold`. Candidates are sorted by
`score * category_weight * epistemic_weight` and selected greedily until the token budget
is exhausted or all candidates are processed. `diversity_bias` penalizes subsequent candidates
from the same category.

## Atlas Markdown Import (`knowledge.import`)

Atlas markdown format:

```
---
id: retrieval.rope
name: Rotary Position Embeddings
tags: [retrieval, transformers]
properties:
  owner: research
---
# Title

Optional pre-section body text.

## Section Heading

Section content...
```

The parser (`parse_atlas_md`) reads the `# Title` line as the atom name, collects text before
the first `##` heading as the atom body, and maps each `##` heading to a `SectionType`. A heading
that ends in a `{type}` marker declares its type: the marker must be a canonical snake_case
`SectionType` value (`## Why memoization terminates {core_model}`), aliases are not accepted inside
the braces, and the marker is removed from the stored heading. Brace text that is not a trailing
lowercase token stays part of the heading. Headings without a marker go through
`SectionType::from_str_loose` (which accepts common heading aliases for the eight section types).
A section whose heading resolves to no type, whether its marker names no section type or no alias
matches, is never guessed: it is not imported, it is counted in `sections_unknown_type` and logged
with its file and heading. The retired `references` and `other` types are not section types, so a
`## References` heading is refused this way, and an imported atom always carries
`source_type = "imported"`.

Optional delimiter-bounded YAML frontmatter is removed before markdown parsing and content
storage. `id`, `atlas_id`, and `atlas-id` are agreeing aliases for canonical identity; when
present, the normalized ID supplies the slug and the original value is stored in
`properties.atlas_id` and `source_uri = atlas:<id>`. `name` (then `title`), `tags`, and nested
`properties` map to their atom fields, while other top-level metadata is retained in properties.
Every import stores the original root-relative markdown path in `properties.source_path`.
Without a canonical ID, identity falls back to the path and `source_uri` is
`file:<source_path>` (a legacy loose `atlas_id:` hint may still supply Atlas provenance without
changing the path-derived slug).

The chunk strategy `"section"` (default) creates one atom + N sections per file.
The `"atom"` strategy creates one atom with the byte-exact UTF-8 post-frontmatter body as
`content` and no sections. Without frontmatter, that body is the complete file.

When frontmatter has no canonical ID, directory slugs are stable root-relative identities:
normalized path components join with `--` (`guides/rope.md` becomes `guides--rope`). A direct
file keeps its normalized stem. Any final-slug collision fails before writes and names both paths.
A canonical re-import updates the same slug/UUID; an identity already claimed by another live slug
is refused rather than duplicated. Discovery is deterministic, does not follow symlinks, and fails
closed at 32 directory levels, 100,000 entries, or 10,000 markdown files. A root directory symlink
is rejected even with a trailing separator. Limit errors name the exact failing path and report the
current/configured depth, entry, and markdown-file counts. The response retains `imported_atoms`,
`imported_sections`, and `files_processed`, and adds `entries_visited`, `files_discovered`,
`files_skipped`, `traversal_errors`, `sections_discovered`, `sections_skipped` (sections under the
minimum content length), and `sections_unknown_type`.

## Numeric Validation

All public request float fields (`min_score`, `intersection_bonus`, `rerank_alpha`,
`diversity_bias`, `epistemic_weight`, `category_weights.*`, `candidates[*].score`,
`candidates[*].information_gain`, `weights.*`) are validated with `is_finite()` at the handler
boundary before being cast to `f32`. Non-finite values return `RuntimeError::InvalidInput`.

## Suggest query admission

`knowledge.suggest` requires at least five words of orientation context. Each
whitespace-separated piece without CJK characters counts as one word, preserving
existing Latin-query behavior. Han, Hiragana, Katakana, and Hangul letters in the
shared CJK script ranges count as one word per two letters; the total is rounded
down once across the entire query. Punctuation within these CJK script ranges does not contribute letters
or an additional whitespace word. A piece containing CJK does not also count as
a separate Latin word.

For example, `如何提升中文检索质量` has ten CJK letters and is admitted;
`如何提升中文检索质` has nine and is refused. `如何提升中文检索质 retrieval`
has four CJK-derived words plus one whitespace word and is admitted. Short queries
can use `knowledge.search`.

Auto-`knowledge.compose` uses the same counting rule with a ten-word minimum, so
an unspaced query needs twenty CJK letters. Providing explicit `domain_ids` or
`atom_ids` bypasses that minimum; a nonempty query is still required. The thresholds
remain five words for suggest and ten for auto-compose. Retrieval ranking and
embedding-model selection are unchanged.
