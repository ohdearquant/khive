# ADR-157: Canonicalize Verified Supersession Chains Before Memory Recall Scoring

**Status**: proposed
**Date**: 2026-08-15
**Scope**: ownership-bounded `memory.remember` supersession capture, ADR-046 staged capture,
runtime-reserved edge governance attribution, offline edge backfill, and the internal
`memory.recall` pipeline. The public recall contract remains unchanged, and the generic graph
edge contract is not narrowed.

## Context

A production-derived labelled evaluation of `memory.recall` (42 real directive-recall
queries; 22 with a labelled correct answer in the store) measured nDCG@10 0.673, with the
correct answer in the top 10 for 20 of 22 answerable queries. Both complete misses shared
one failure mechanism: the stored correct answer was a correction note that intentionally
replaced an earlier note, the replacement was recorded only in note text rather than as a
`supersedes` edge, and the terse correction lost lexically to the older, vocabulary-richer
note it replaced. The remaining 20 of 42 queries had no stored answer at all — a
persistence gap outside retrieval's reach and explicitly out of scope here.

Today the recall pipeline applies supersession only as binary suppression after scoring.
It gathers all ranked candidate IDs into one `EdgeFilter`, pages that candidate-batched
filter to exhaustion, unions the matched target IDs with a `properties.supersedes`
shortcut, and removes those IDs. This is already a batched exclusion operation, not a
per-candidate graph access pattern. It cannot substitute a chain head that retrieval did
not rank or preserve a matched member's fused retrieval evidence for that head. A previous
attempt at additive temporal scoring was reverted because recency terms outweighed
semantic relevance on unrelated queries; any mechanism here must not reintroduce that
class.

## Teardown

- **Two misses are too few for a change.** They are too few for a general ranking policy,
  but they are 2/22 — 9.1% — of queries with a stored correct answer, and both share one
  failure mechanism. That supports a narrow correctness mechanism, not a ranking program.
- **Write-time edges plus binary filtering are sufficient.** No. Write-time capture is
  prospective; filtering can remove obsolete candidates but cannot guarantee that a weaker
  successor enters the served set.
- **An edge alone enables chain-head replacement.** No. The edge must be governed, a chain
  member must be present in the pre-final candidates, every traversed edge and node must
  remain in that candidate's namespace, and the chain must have one live memory head created
  no later than the query start.
- **Correction-like text is trustworthy structure.** No. Negation, quotations, malformed
  IDs, and scope differences can fabricate graph truth. Text may support a curation
  proposal, never automatic edge creation.
- **Cluster recency safely approximates supersession.** No. Similarity does not establish
  replacement, and it cannot recover targets absent from the served pools.
- **A reranker feature is the smallest change.** Under ADR-033, feature reranking is
  optional and replaces default scoring when configured. Verified truth must not depend on
  an optional weight.
- **Corpus check.** The prior-decision search was inconclusive at authoring time; the
  acceptance check must confirm no conflicting accepted ADR was missed.

## Decision

Treat supersession as verified truth canonicalization, not as a general ranking signal.
Only **GOVERNED** supersedes edges participate: the runtime must have stamped the edge's
reserved `metadata.created_by_actor` key from resolved identity on either the
ownership-bounded `memory.remember` direct path or the ADR-046 path after different-actor
approval. Canonicalization begins with an edge set filtered to governed edges in the
candidate's write namespace. Cross-namespace and ungoverned edges are absent from closure
input, exactly as if they did not exist. It then follows only live (non-deleted) memory
nodes, selects only a live memory head, and excludes every note or edge created after the
recall query's start snapshot. No classification, degradation, suppression, or substitution
may branch on an ungoverned edge.

Adopt governed write-time capture, governed historical backfill, and one batched
governed-edge chain-head substitution. Do not adopt cluster recency or reranker features
from this evidence.

### Component boundary

```mermaid
flowchart LR
  W["memory.remember request"] --> G["Authorization Gate"]
  P["ADR-046 approved proposal"] --> G
  L["generic link request"] --> G
  G --> D{"Capture path"}
  D -->|direct| O["Direct-path ownership floor"]
  D -->|staged| A["Different-actor approval proof"]
  D -->|generic| V
  O --> V["Supersession invariant validation"]
  A --> V
  V --> E{"Governed path?"}
  E -->|yes| T["Runtime governance stamp"]
  E -->|no| S[("Notes and graph-visible edges")]
  T --> S
  R["Fused recall candidates"] --> C["Governed-edge closure filter"]
  S --> C
  C --> Q["Canonical substitution and scoring"]
```

The Gate is the authorization seam and may apply stricter deployment policy. The
direct-path ownership check is the minimum prevention rule guaranteed by this ADR, not a
Gate policy. The write validator enforces data-integrity invariants, while the closure
query applies serving predicates without changing stored records.

A supersedes edge is **GOVERNED** only when `metadata.created_by_actor` was written
server-side from the path's resolved runtime identity after one of those two proofs. Merely
having the same source, target, relation, namespace, or apparent metadata value does not
grant governance.

### 1. Capture supersession atomically

`memory.remember` may gain an optional, bounded `supersedes: [<full-memory-uuid>, ...]`
field.

Edges are directed `new --supersedes--> old`. Every target must be a live (non-deleted)
memory note in the caller's resolved write namespace, which is also the new note's
namespace. On this direct path, every target must also carry durable
`metadata.created_by_actor` exactly equal to the caller's resolved actor identity. A target
outside that namespace, a target attributed to another actor, or a target without that
metadata is refused as `invalid_supersedes`; namespace visibility alone is not sufficient.
The refusal payload includes `staged_path: "ADR-046 proposal lifecycle"`. The write must
not create a cycle. The note and all declared edges commit atomically; failed validation
or edge creation commits neither.

Cross-actor and legacy-target supersession uses the existing ADR-046 proposal lifecycle.
A change-set proposing the `supersedes` edge lands only after approval by an actor other
than the proposer. That staged path supplies mandatory review; this ADR does not duplicate
or replace ADR-046's lifecycle mechanics. At apply time, that governed path stamps the edge's
`metadata.created_by_actor` server-side from the resolved apply identity. Legacy rows without
durable ownership metadata are never backfilled with guessed ownership.

The write remains authorized at the existing ADR-018 Gate seam. Its `GateRequest` carries
the resolved caller actor, the resolved write namespace, and the unchanged target IDs in
the request arguments, so deployed policy can evaluate all three. After the Gate allows
dispatch, the handler enforces the ownership floor plus the same-namespace, liveness,
memory-kind, and cycle invariants. Gate policy may tighten access further, but it may not
relax this ADR-level floor. These checks do not create a new capability system and are not
storage authorization checks.

Every note created by `memory.remember` carries durable
`metadata.created_by_actor = {"kind": <kind>, "id": <id>}` attribution copied from the
resolved caller identity. Each directly created governed edge carries the same attribution
shape and the write namespace, stamped server-side only after the ownership floor succeeds.

`metadata.created_by_actor` is a **RUNTIME-RESERVED edge-metadata key**. Generic edge write
surfaces, including `link` and edge `update`, must reject any caller-supplied value for that
key, including `null` or a removal request, as `reserved_edge_metadata_key`; they must not
silently strip it. The two governed paths are the only writers and derive the value from
resolved runtime identity rather than request content. An edge update that does not name the
key preserves any existing runtime value. This is metadata input validation, not
endpoint-contract tightening. [ADR-017](ADR-017-pack-standard.md)
§“Pack-extensible edge endpoints” makes endpoint rules additive only: packs cannot tighten
ADR-002's base contract and can only add legal pairs. Edge creation therefore continues
through the centralized endpoint validator, and a contract test must prove that the
composed ADR-002 base rules and loaded ADR-017 `EDGE_RULES` accept
memory-note-to-memory-note `supersedes`; no handler-local bypass is permitted. Because the
endpoint contract cannot be narrowed, recall authority is enforced by the reserved stamp
and the serving-side predicate instead.

A `supersedes` edge created through generic `link` remains legal graph data under that base
contract. It remains visible to graph queries and traversal and may preserve historical
meaning, but, because it lacks the runtime stamp, it has no recall-canonicalization
authority. Recall substitution is a view-layer privilege granted only to governed edges.
This forward-only attribution rule requires no migration and does not infer authorship for
legacy notes or governance for existing edges.

The request field is an edge-creation instruction, not a second authority marker in note
properties. A governed supersedes edge can be removed through the existing by-ID curation
surface, `delete(id=<edge-uuid>)`, which remains subject to the existing Gate. The next
recall snapshot then excludes that edge and restores the prior eligible head. This is the
recovery path for an incorrect or malicious governed supersession; no separate unsupersede
verb is introduced. Deleting an ungoverned edge changes graph history but cannot change
recall canonicalization.

Correction-text detection may emit diagnostics or proposals. It may not mint edges or
affect ordering.

### 2. Backfill through curation

An offline campaign may scan historical notes for supersession claims. Each scan is
bounded to live (non-deleted) notes of kind `memory` in one captured namespace snapshot.
Both the proposed source and target must be live memory notes in that same namespace. The
campaign produces dry-run proposals containing the source note, resolved target, evidence
span, resolution method, and per-edge annotation.

Backfill reads and proposals use same-namespace evidence only. A legacy property reference
to a note in another namespace is reported as a diagnostic; it does not produce a proposal
or edge and never participates in serving suppression.

Full UUIDs may be proposed directly. Short IDs are eligible only when uniquely resolved
against the captured namespace snapshot. Ambiguous claims remain observations. Only
ADR-046 proposals approved by an actor other than the proposer create edges. Approved
writes traverse the same Gate and atomic data-integrity validation as explicit capture;
the direct-path target-ownership check does not apply because the different-actor approval
is the staged authorization for legacy or cross-actor targets. The proposal apply path
stamps each resulting edge's reserved `metadata.created_by_actor` server-side from its
resolved apply identity, so the backfilled edge is governed and eligible for recall
canonicalization. A dry-run result, unapproved proposal, legacy property, generic-link edge,
or edge without that runtime stamp remains ungoverned and absent from closure input.

Complexity is `O(N × L + P)` offline, with no recall hot-path cost.

### 3. Canonicalize chains in one batch

After fusion and before note-local scoring or optional reranking:

1. At query entry, open the consistent read snapshot used for candidate hydration and
   closure, and read the storage query-start timestamp `Tq` from that snapshot before
   candidate retrieval.
2. Collect at most `C ≤ 200` candidates that are live (non-deleted) notes of kind `memory`,
   belong to the recall visible set, and have `created_at ≤ Tq`. Retain each candidate's
   write namespace as part of the closure input.
3. In one database query against that snapshot, fetch the bounded supersession closure.
   Before traversal or classification, apply the three closure-traversal exclusion classes
   together: **cross-namespace**, **ineligible-node**, and **ungoverned**. An edge is
   cross-namespace when its namespace differs from the candidate namespace or either
   endpoint is outside that namespace. An edge is ungoverned unless it carries the
   runtime-reserved `metadata.created_by_actor` stamp produced by a governed path. The query
   predicates do not return cross-namespace or ungoverned edges, exactly as if they did not
   exist; neither class may affect classification, degradation, suppression, or output. An
   edge incident to an ineligible node is excluded from traversal and head selection and may
   be returned only as the eligibility marker described in step 4.
4. Within that governed, namespace-filtered edge set, follow eligible incoming edges toward
   newer notes, with depth 16 and 800 expanded-node caps. Every traversed edge must be live,
   governed, and have `created_at ≤ Tq`; every traversed source, intermediate node, target,
   and selected head must be a live (non-deleted) note of kind `memory` with
   `created_at ≤ Tq`. The query may return a governed, same-namespace edge incident to an
   ineligible node only as an eligibility marker for step 5; that node is never traversed or
   emitted.
5. Map each valid component to its unique eligible head. Classify forks and unavailable
   heads only from the governed, namespace-filtered edge set. A same-namespace head reached
   by a governed edge that is deleted, is not a memory note, or was created after `Tq` is
   head-unavailable and is never emitted. A candidate whose only supersession evidence is
   cross-namespace or ungoverned remains a one-node component and is emitted normally as its
   own head.
6. Preserve the best fused retrieval evidence from matched members.
7. Take salience, decay, content, timestamps, and every other note-local scoring feature
   only from the eligible head selected from the query-start snapshot.
8. Deduplicate before scoring.

This is pointer substitution licensed by a governed explicit relation — not a recency
boost.

The governed, same-namespace closure is a serving/view-layer predicate consistent with
ADR-007's attribution-only namespace model and the repository's data-vs-view principle. It
restricts what `memory.recall` may substitute; it does not reject or remove cross-namespace
or ungoverned graph data, make namespace an authorization boundary, narrow the generic
`link` endpoint contract, or change namespace-agnostic by-ID operations. Generic-link
`supersedes` edges remain visible to graph queries and traversal and retain their historical
meaning, but carry no recall-canonicalization authority. Serving never classifies, degrades,
suppresses, or otherwise branches on a cross-namespace or ungoverned edge because that edge
is absent from closure input. Multiple visible namespaces are canonicalized as independent
components.

Graph work is one round trip and `O(C + E_b)`. One bounded governed-edge closure query
replaces the current candidate-batched, paginated exclusion query; there may be no
per-candidate fallback. The legacy `properties.supersedes` shortcut does not license
substitution after activation. A curated legacy property must first become a governed,
runtime-stamped edge through the ADR-046 backfill path, which keeps edge deletion
authoritative for recovery.

### Serving sequence

```mermaid
sequenceDiagram
  participant Recall as memory.recall
  participant Store as Storage snapshot
  participant Score as Scoring pipeline
  Recall->>Store: Open snapshot and capture Tq
  Recall->>Store: Retrieve and hydrate eligible candidates
  Recall->>Store: One governed-edge closure query (IDs, namespaces, Tq)
  Store-->>Recall: Governed, namespace-filtered components and eligibility markers
  Recall->>Recall: Select heads, degrade, and deduplicate
  Recall->>Score: Fused evidence plus head-local features
```

### 4. Degradation contract

The existing recall audit payload gains additive degradation modes; the public recall
result shape remains unchanged. Each mode has a named consumer and must not ship until
that consumer recognizes it. The table is evaluated only over governed, same-namespace
edges after node and `Tq` eligibility checks. Cross-namespace and ungoverned edges never
reach these conditions, emit no degradation mode, and cannot affect suppression or output.

| Condition                                                                                                                                         | Behavior                                                                                      | Mode                            | Required consumer                                       |
| ------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------- | ------------------------------- | ------------------------------------------------------- |
| Batch governed-edge closure query fails                                                                                                           | Return baseline ranking without canonicalization; no per-candidate retry                      | `supersession_lookup_failed`    | `RecallExecuted` projection and frozen replay evaluator |
| Unique same-namespace head in the governed, namespace-filtered edge set is deleted, non-memory, or post-`Tq`                                      | Suppress known superseded members; fill from unaffected candidates                            | `supersession_head_unavailable` | `RecallExecuted` projection and frozen replay evaluator |
| Multiple eligible heads in the governed, namespace-filtered edge set                                                                              | Inject no branch; suppress non-heads; independently retrieved eligible heads compete normally | `supersession_fork`             | `RecallExecuted` projection and frozen replay evaluator |
| Cycle or traversal cap in the governed, namespace-filtered edge set                                                                               | Suppress the affected component                                                               | `supersession_chain_invalid`    | `RecallExecuted` projection and frozen replay evaluator |
| Invalid direct-path write, including a target outside the caller's write namespace, foreign-authored target, or target without ownership metadata | Commit neither note nor edges; return the ADR-046 staged-path pointer                         | `invalid_supersedes`            | `memory.remember` error mapper and Gate audit stream    |
| Caller supplies `metadata.created_by_actor` to generic `link` or edge `update`, including `null` or removal                                       | Reject the request without stripping or mutating                                              | `reserved_edge_metadata_key`    | KG input-error mapper and Gate audit stream             |
| Ambiguous or ineligible backfill                                                                                                                  | Create no edge; retain the proposal record                                                    | `backfill_ambiguous`            | backfill proposal processor                             |

### 5. Activation gate

Replay the frozen evaluation pools with a frozen, curated edge overlay. Each replay query
must use its captured `Tq` and a consistent snapshot. The evaluator may introduce a target
only through deterministic substitution from a captured candidate chain member and only
when the edge is governed and the edge, every intermediate node, and the head satisfy the
same-namespace, live-memory, and `created_at ≤ Tq` predicates. It may not rerun retrieval,
and its overlay may not grant governance to an edge lacking the runtime stamp.

Activation requires:

- both supersession hard misses become top-10 correct answers;
- no existing top-10 correct answer falls out across the 22 answerable queries;
- nDCG@10 remains at least 0.673;
- the 20 no-stored-answer cases remain classified as persistence failures;
- exactly one governed-edge closure query at `C = 200`, compared with the existing one
  batched exclusion query;
- recall p50 and p95 within a predeclared 5% non-inferiority margin;
- an ownership-refusal test proving that both a foreign-actor target and a legacy target
  without `created_by_actor` refuse the direct path as `invalid_supersedes`, commit neither
  note nor edges, and return the ADR-046 staged-path pointer;
- a staged-path acceptance test proving that an ADR-046 proposal carrying `supersedes`
  edges lands after approval by an actor different from the proposer and receives the
  runtime governance stamp at apply time;
- a cross-namespace-inertness test proving that adding a hostile cross-namespace edge
  produces byte-identical recall output to the same snapshot without that edge;
- a generic-link inertness test proving that adding a same-namespace memory-to-memory
  `supersedes` edge through generic `link` changes nothing in recall output, which must be
  byte-identical to the same snapshot without that edge;
- reserved-key rejection tests proving that generic `link` and edge `update` each refuse a
  caller-supplied `metadata.created_by_actor` value as `reserved_edge_metadata_key`,
  including `null` and removal attempts, without silently stripping or mutating;
- governed-path acceptance tests proving that a runtime-stamped edge from the
  ownership-bounded `memory.remember` path and a runtime-stamped edge from the ADR-046
  different-actor-approved path are each honored by canonicalization;
- test coverage of chains, duplicate mappings, forks, cycles, deleted/non-memory/post-`Tq`
  same-namespace heads, unavailable heads, ungoverned-edge inertness, edge-delete recovery,
  closure-query failure, atomic rollback, and a write rejected by the Gate;
- an endpoint-contract test through `validate_edge_relation_endpoints` with the memory pack
  loaded, proving generic memory-to-memory `supersedes` remains legal but ungoverned; and
- a consumer test for every degradation mode named above.

If both misses are not recovered, do not enable substitution. Record the result as
candidate-generation or persistence evidence; do not compensate with cluster recency.

## Ceiling analysis

Baseline: nDCG@10 0.673, correct answer in top-10 for 20/22, at rank 1 for 7/22.

A mechanism affecting only the two misses can improve mean nDCG by at most `2/22 = 0.091`,
giving a loose ceiling of 0.764. Its top-10 ceiling is 22/22 and, if only those two reach
rank 1, its top-1 ceiling is 9/22.

| Option                                   | Recovery ceiling on the frozen set                                                             | Complexity                                                  | Verdict                                 |
| ---------------------------------------- | ---------------------------------------------------------------------------------------------- | ----------------------------------------------------------- | --------------------------------------- |
| Governed write-time linking              | 0/2 historical misses                                                                          | `O(S)`, `S ≤ 16`; existing batched suppression remains      | Adopt explicit input only               |
| Governed-edge chain resolution           | 0/2 without governed edges or a candidate chain member; conditionally 2/2                      | One `O(C + E_b)` closure query replaces one exclusion query | Adopt with linking + backfill and proof |
| Cluster recency                          | 0/2 hard misses; could at most move 13 reachable non-top-1 targets                             | `O(C²d)` pairwise                                           | Reject                                  |
| Reranker feature                         | 0/2 if scoring existing candidates; head injection is chain resolution                         | `O(C)` after batched metadata                               | Reject                                  |
| Backfill alone                           | Guaranteed 0/2 on frozen served pools; theoretical ≤2/2 only from a wider hidden candidate set | Offline `O(NL + P)`                                         | Necessary but insufficient              |
| Governed linking + backfill + resolution | Conditional 2/2; nDCG ≤ 0.764, top-10 ≤ 22/22                                                  | Bounded write plus one recall closure query                 | Chosen, activation-gated                |

## Alternatives considered

- **Reject memory-to-memory `supersedes` on generic `link`:** unavailable because it would
  tighten ADR-002's base endpoint contract, while ADR-017 permits pack endpoint rules to add
  legal pairs only. Preserve the graph edge and withhold view-layer authority instead.
- **Governed linking + backfill + current binary filter:** cheapest fallback, but cannot
  guarantee successor promotion and remains suppression-only.
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
intentionally replaces another. Only a governed instance of the latter licenses recall
substitution.

Governed direct capture prevents new unlinked chains, governed backfill repairs historical
structure, and batched governed-edge chain resolution prevents a lexically rich obsolete
note from eclipsing its terse successor. The closure retains the current batched
graph-access shape while upgrading the operation from exclusion to bounded canonical
substitution.

Ownership bounds the direct path to corrections of the caller's own attributed notes.
ADR-046 supplies different-actor approval when ownership is absent or belongs to another
actor. Both paths stamp governance attribution at runtime. The governed, namespace-filtered
closure prevents stored cross-namespace or ungoverned data from influencing serving while
leaving generic supersession edges available as historical graph data.

This remains a data-integrity intervention with a serving canonicalizer — not a general
ranking program.

## Implementation fences

### MAY

- Add bounded, optional, atomic supersession capture to `memory.remember`.
- Stamp `metadata.created_by_actor` server-side on governed direct and approved-proposal
  edges, and reject caller-supplied values for that runtime-reserved key.
- Use text detection for warnings, observations, or proposals.
- Replace the candidate-batched binary edge/property suppression with one bounded batch
  governed-edge closure.
- Add non-breaking internal audit and diagnostic fields.

### MAY NOT

- Change the public `memory.recall` request or normal response shape.
- Add global additive recency, unscoped recency multipliers, or cluster recency.
- Create edges from text alone.
- Query the graph per candidate, including during degradation.
- Make verified supersession depend on reranker configuration.
- Admit an edge to closure input unless it is governed and its namespace and both endpoint
  namespaces equal the root candidate's namespace, or let a cross-namespace or ungoverned
  edge affect classification, degradation, suppression, or output.
- Traverse an ineligible node, emit a deleted or non-memory head, or credit a note or edge
  created after `Tq`.
- Treat the governed, same-namespace serving predicate as storage isolation,
  authorization, endpoint-contract tightening, or a restriction on by-ID operations.
- Reject a generic memory-to-memory `supersedes` edge solely because it is ungoverned,
  hide it from graph queries or traversal, or grant it recall-canonicalization authority.
- Accept, silently strip, or persist caller-supplied `metadata.created_by_actor` on generic
  `link` or edge `update`.
- Permit direct supersession of a target whose durable creator identity is absent or does
  not equal the caller's resolved identity; those targets require the ADR-046 staged path.
- Backfill legacy ownership metadata by inference or guesswork.
- Claim improvement for the 20 no-stored-answer queries.

### Verify by

- Frozen evaluation replay under the governed-edge activation gate above.
- Query-count instrumentation at `C = 1, 10, 100, 200`.
- Latency comparison against the existing candidate-batched exclusion operation.
- Determinism, direct-path ownership refusal, different-actor staged approval, governed-edge
  acceptance from both paths, cross-namespace and generic-link byte-identical inertness,
  live-memory, query-snapshot, edge-delete recovery, and degradation-consumer tests.
- Named `reserved_edge_metadata_key` rejection tests for caller-supplied governance
  attribution on generic `link` and edge `update`.
- Central endpoint-rule legality verification before dependent implementation merges,
  proving the generic edge remains legal while serving ignores it unless governed.
