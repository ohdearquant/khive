# ADR-157: Canonicalize Verified Supersession Chains Before Memory Recall Scoring

**Status**: proposed
**Date**: 2026-08-15
**Scope**: `memory.remember` supersession capture, offline edge backfill, and the internal `memory.recall` pipeline. The public recall contract remains unchanged.

## Context

A production-derived labelled evaluation of `memory.recall` (42 real directive-recall
queries; 22 with a labelled correct answer in the store) measured nDCG@10 0.673, with the
correct answer in the top 10 for 20 of 22 answerable queries. Both complete misses shared
one failure mechanism: the stored correct answer was a correction note that intentionally
replaced an earlier note, the replacement was recorded only in note text rather than as a
`supersedes` edge, and the terse correction lost lexically to the older, vocabulary-richer
note it replaced. The remaining 20 of 42 queries had no stored answer at all — a
persistence gap outside retrieval's reach and explicitly out of scope here.

Today the recall pipeline reads `supersedes` edges only as a binary exclusion filter,
implemented as one graph query per candidate (an N+1 access pattern), and no edge
influences ranking. A previous attempt at additive temporal scoring was reverted because
recency terms outweighed semantic relevance on unrelated queries; any mechanism here must
not reintroduce that class.

## Teardown

- **Two misses are too few for a change.** They are too few for a general ranking policy,
  but they are 2/22 — 9.1% — of queries with a stored correct answer, and both share one
  failure mechanism. That supports a narrow correctness mechanism, not a ranking program.
- **Write-time edges plus binary filtering are sufficient.** No. Write-time capture is
  prospective; filtering can remove obsolete candidates but cannot guarantee that a weaker
  successor enters the served set. It also preserves the current N+1 graph-query cliff.
- **An edge alone enables chain-head replacement.** No. A chain member must be present in
  the pre-final candidates, the chain must have one visible head, and that head must
  satisfy the evaluation timestamp.
- **Correction-like text is trustworthy structure.** No. Negation, quotations, malformed
  IDs, and scope differences can fabricate graph truth. Text may support a review
  proposal, never automatic edge creation.
- **Cluster recency safely approximates supersession.** No. Similarity does not establish
  replacement, and it cannot recover targets absent from the served pools.
- **A reranker feature is the smallest change.** Under ADR-033, feature reranking is
  optional and replaces default scoring when configured. Verified truth must not depend on
  an optional weight.
- **Corpus check.** The prior-decision search was inconclusive at authoring time;
  sign-off review must confirm no conflicting accepted ADR was missed.

## Decision

Treat supersession as verified truth canonicalization, not as a general ranking signal.

Adopt explicit write-time capture, reviewed historical backfill, and one batched
serve-time chain-head substitution. Do not adopt cluster recency or reranker features from
this evidence.

### 1. Capture supersession atomically

`memory.remember` may gain an optional, bounded `supersedes: [<full-memory-uuid>, ...]`
field.

Edges are directed `new --supersedes--> old`. Every target must be a visible memory note
in the same write namespace, and the write must not create a cycle. The note and all
declared edges commit atomically; failed validation or edge creation commits neither.

Correction-text detection may emit diagnostics or proposals. It may not mint edges or
affect ordering.

### 2. Backfill through review

An offline campaign may scan historical notes for supersession claims. It produces
dry-run proposals containing the source note, resolved target, evidence span, resolution
method, and per-edge annotation.

Full UUIDs may be proposed directly. Short IDs are eligible only when uniquely resolved
against the captured namespace snapshot. Ambiguous claims remain observations. Only
approved proposals create edges.

Complexity is `O(N × L + P)` offline, with no recall hot-path cost.

### 3. Canonicalize chains in one batch

After fusion and before note-local scoring or optional reranking:

1. Collect at most `C ≤ 200` candidate IDs.
2. Fetch their bounded supersession closure in one database query.
3. Follow incoming edges toward newer notes, with depth 16 and 800 expanded-node caps.
4. Map each valid component to its unique visible head.
5. Preserve the best fused retrieval evidence from matched members.
6. Take salience, decay, content, timestamps, and other note-local features from the head.
7. Deduplicate before scoring.

This is pointer substitution licensed by an explicit relation — not a recency boost.

Graph work is one round trip and `O(C + E_b)`. The current per-candidate graph checks are
removed. There may be no N+1 fallback.

### 4. Degradation contract

The existing recall audit payload gains additive degradation modes; the public recall
result shape remains unchanged.

| Condition                  | Behavior                                                                             | Mode                            |
| -------------------------- | ------------------------------------------------------------------------------------ | ------------------------------- |
| Batch graph query fails    | Return baseline ranking without canonicalization; no N+1 retry                       | `supersession_lookup_failed`    |
| Unique head is unavailable | Suppress known superseded members; fill from unaffected candidates                   | `supersession_head_unavailable` |
| Multiple heads             | Inject no branch; suppress non-heads; independently retrieved heads compete normally | `supersession_fork`             |
| Cycle or traversal cap     | Suppress the affected component                                                      | `supersession_chain_invalid`    |
| Invalid explicit write     | Commit neither note nor edges                                                        | `invalid_supersedes`            |
| Ambiguous backfill         | Create no edge; retain review record                                                 | `backfill_ambiguous`            |

### 5. Activation gate

Replay the frozen evaluation pools with a frozen, reviewed edge overlay. The evaluator may
introduce a target only through deterministic substitution from a captured candidate chain
member. It may not rerun retrieval or admit a note created after the original query.

Activation requires:

- both supersession hard misses become top-10 correct answers;
- no existing top-10 correct answer falls out across the 22 answerable queries;
- nDCG@10 remains at least 0.673;
- the 20 no-stored-answer cases remain classified as persistence failures;
- exactly one graph query at `C = 200`;
- recall p50 and p95 within a predeclared 5% non-inferiority margin; and
- test coverage of chains, duplicate mappings, forks, cycles, unavailable heads, batch
  failure, and atomic rollback.

If both misses are not recovered, do not enable substitution. Record the result as
candidate-generation or persistence evidence; do not compensate with cluster recency.

## Ceiling analysis

Baseline: nDCG@10 0.673, correct answer in top-10 for 20/22, at rank 1 for 7/22.

A mechanism affecting only the two misses can improve mean nDCG by at most `2/22 = 0.091`,
giving a loose ceiling of 0.764. Its top-10 ceiling is 22/22 and, if only those two reach
rank 1, its top-1 ceiling is 9/22.

| Option                                | Recovery ceiling on the frozen set                                                             | Complexity                           | Verdict                                 |
| ------------------------------------- | ---------------------------------------------------------------------------------------------- | ------------------------------------ | --------------------------------------- |
| Write-time linking                    | 0/2 historical misses                                                                          | `O(S)`, `S ≤ 16`; leaves current N+1 | Adopt explicit input only               |
| Chain resolution                      | 0/2 without edges or a candidate chain member; conditionally 2/2                               | One `O(C + E_b)` query               | Adopt with linking + backfill and proof |
| Cluster recency                       | 0/2 hard misses; could at most move 13 reachable non-top-1 targets                             | `O(C²d)` pairwise                    | Reject                                  |
| Reranker feature                      | 0/2 if scoring existing candidates; head injection is chain resolution                         | `O(C)` after batched metadata        | Reject                                  |
| Backfill alone                        | Guaranteed 0/2 on frozen served pools; theoretical ≤2/2 only from a wider hidden candidate set | Offline `O(NL + P)`                  | Necessary but insufficient              |
| Linking + backfill + chain resolution | Conditional 2/2; nDCG ≤ 0.764, top-10 ≤ 22/22                                                  | Bounded write + one recall query     | Chosen, activation-gated                |

## Alternatives considered

- **Linking + backfill + current binary filter:** cheapest fallback, but cannot guarantee
  successor promotion and retains N+1 access.
- **Cluster-scoped multiplicative recency:** might improve reachable ordering, but cannot
  recover absent targets and confuses similarity with authority.
- **ADR-033 chain-position feature:** optional weighting is the wrong contract for
  semantic truth.
- **Automatic text-derived edges:** fastest coverage, unacceptable fabrication risk.
- **No build:** appropriate if the offline gate fails. The aggregate evidence does not
  support a broad ranking project, but two mechanism-identical hard misses justify the
  bounded experiment.

## Rationale

Supersession differs from freshness. Freshness says a note is newer; supersession says it
intentionally replaces another. Only the latter licenses substitution.

Explicit linking prevents new unlinked chains, reviewed backfill repairs historical
structure, and batched chain resolution prevents a lexically rich obsolete note from
eclipsing its terse successor. Batching also removes the existing per-candidate scale
cliff.

This remains a data-integrity intervention with a serving canonicalizer — not a general
ranking program.

## Implementation fences

### MAY

- Add bounded, optional, atomic supersession capture to `memory.remember`.
- Use text detection for warnings, observations, or proposals.
- Replace binary N+1 filtering with one bounded batch closure.
- Add non-breaking internal audit and diagnostic fields.

### MAY NOT

- Change the public `memory.recall` request or normal response shape.
- Add global additive recency, unscoped recency multipliers, or cluster recency.
- Create edges from text alone.
- Query the graph per candidate, including during degradation.
- Make verified supersession depend on reranker configuration.
- Cross namespace boundaries or credit post-query notes.
- Claim improvement for the 20 no-stored-answer queries.

### Verify by

- Frozen evaluation replay under the activation gate above.
- Query-count instrumentation at `C = 1, 10, 100, 200`.
- Latency comparison against the removed N+1 implementation.
- Determinism and degradation-path tests.
- Design sign-off before dependent implementation merges.
