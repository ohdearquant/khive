# ADR-087: Substrate-Kind Federated Search — Coordinator Fan-Out with Unweighted RRF

**Status**: proposed\
**Date**: 2026-05-22\
**Authors**: Ocean, lambda:khive\
**Depends on**: ADR-019 (Note Kind Taxonomy — substrate vs granular kinds), ADR-024
(Cross-Substrate Search Contract), ADR-078/ADR-081 (multi-engine — engine weights operate
within a backend), ADR-079 (Pack-Scoped Backends)\
**Part of**: ADR-080 (SubstrateCoordinator umbrella)

## Context

ADR-024 establishes that `search(kind=note, query=...)` returns every note matching the query,
regardless of which pack created it. ADR-079 partitions notes (and entities, edges, events)
across multiple backends. The contracts are in tension: if `memory` is on `main.db` and `lore`
is on `lore.db`, a single `search(kind=note)` must touch both backends and merge the result.

The same applies to every substrate-kind verb — `list(kind=entity, ...)`, `get(kind=event,
id=...)` — but search is the most consequential because it returns a _ranked_ result list, and
ranking across backends needs a defensible fusion policy.

This ADR makes one decision: **how the kernel coordinator federates substrate-kind search and
fuses per-backend ranked results**. Granular kinds (`task`, `memory`) — pack-owned per ADR-025
and ADR-026 — stay pack-local and do not federate.

## Decision

### D4 — Substrate-kind search fuses with unweighted RRF across backends

The kernel coordinator (see ADR-080 umbrella) maintains a map:

```text
SubstrateKind → Vec<Arc<KhiveRuntime>>
```

Each substrate kind lists the runtimes (and therefore the backends) hosting that kind. For
`search(kind=note, query=...)`:

1. The coordinator looks up the runtimes hosting `note` and fans out the search to each.
2. Each runtime executes a single-backend `search()` (its existing path, modified per ADR-083
   to take pre-computed embeddings).
3. The per-backend results are ranked lists of `(uuid, score)` pairs.
4. The coordinator fuses the per-backend lists using
   `khive_fusion::FusionStrategy::Rrf { k: DEFAULT_RRF_K }` — **unweighted**.
5. The fused result is truncated to the requested `top_k` and returned.

#### Why unweighted RRF

Backends are isolation boundaries, not relevance signals. A user's `main` and `lore` backends
each store notes; neither is intrinsically "more authoritative" at the backend level. The
relevance signals that DO matter — model quality (per ADR-078: BGE vs mE5 vs Qwen3) and lexical
match strength (FTS5 BM25) — operate **inside** a single backend, where the comparisons are
calibrated and the question "which signal contributed more to this hit?" has a measurable
answer.

A per-backend `weight` config field would invite operators to set weights guessing what they
mean, with no measurable effect on retrieval quality. Removing the knob removes the footgun.
Engine weights inside each backend (ADR-082 calibration) are the right place to tune.

#### Why fusion is RRF (not raw score)

Per-backend scores are not directly comparable. Backend A's top hit at score 0.84 from a BGE
hybrid pipeline is not the same scale as Backend B's top hit at score 0.79 from an mE5 pipeline
or a different fusion strategy inside that backend. RRF uses **rank position only**, sidestepping
the scale problem and matching the existing single-backend fusion already used in ADR-081's
pack handlers.

`DEFAULT_RRF_K` is the same constant `khive-fusion` uses for engine-level fusion (k=60 per
existing implementation); reusing it keeps the constants table small.

#### Substrate vs. granular: only substrate-kind search federates

ADR-019 and ADR-025 distinguish substrate kinds (`note`, `entity`, `edge`, `event`) from
pack-owned granular kinds (`task` per ADR-026, `memory` per ADR-036, future pack kinds).

- `search(kind=note)` — substrate kind. Federates across all backends hosting notes.
- `search(kind=task)` — granular kind owned by gtd-pack. Goes to whichever backend gtd is
  assigned to (single backend per ADR-079 D6). No federation.
- `search(kind=memory)` — granular kind owned by memory-pack. Single backend; no federation.

The coordinator dispatches by kind: substrate-kind verbs route to the coordinator; granular-kind
verbs route directly to the owning pack's runtime. ADR-088's operation matrix lists every
verb-kind combination.

#### Plan shape

The coordinator's substrate search input is verb-aligned, not query-language-shaped:

```text
SubstrateSearchPlan {
    query_text: Option<String>,
    query_vec: Option<(model_id, Vec<f32>)>,   // caller pre-computed per ADR-083
    top_k: u32,
    min_score: Option<DeterministicScore>,
    target_backends: Option<Vec<String>>,       // None = all hosting backends
    fusion: FusionStrategy,                      // defaults to Rrf {k: DEFAULT_RRF_K}
}
```

The `fusion` field permits overriding the default for callers with a measurable reason; the
default is the recommendation.

## Why no QueryPlan / cost estimator

RuVector's `QueryPlan { target_shards, steps, estimated_cost, is_distributed }` exists because
RuVector parses Cypher-like queries and decides execution order. khive verbs are **already
operationally typed** — `search(kind=note)` is one step (fan-out + fuse), not a multi-step plan
needing optimization.

The coordinator's job is verb dispatch, not query optimization. If a future ADR introduces a
query language (Cypher/GQL/SPARQL surface at the kernel level), it will need its own planner;
this ADR explicitly does not introduce one.

## Single-backend default behavior

With one `[[backends.main]]` entry hosting all packs:

- The `SubstrateKind → Vec<Runtime>` map for `note` has one entry.
- Fan-out has one target backend.
- RRF over one ranked list is identity (rank 1 stays rank 1).
- Result is bit-identical to the pre-ADR-080 single-backend behavior.

The coordinator is zero-cost on the common deployment shape.

## Alternatives considered

### A. Per-backend weighted RRF

Configure a `weight` per backend in `khive.toml`; substrate search fuses with
`FusionStrategy::Weighted`. Rejected: backends are isolation boundaries, not relevance signals;
the weight has no calibration target. See "Why unweighted RRF" above.

### B. Truncate per-backend before fusing

Fetch top-K from each backend, fuse the union. Rejected: K is shared across backends, so
high-quality hits beyond rank K in backend A would be lost. The current proposal fetches a
candidate pool per backend (per the same multiplier used in single-backend hybrid search) and
fuses the union.

### C. Don't federate — return per-backend grouped results

Return `Vec<(BackendName, Vec<SearchHit>)>` and let the caller decide what to do. Rejected:
ADR-024's contract is "search all notes," not "search per backend." Pushing the merge to the
caller breaks the abstraction.

### D. Re-rank with a cross-encoder

Add a top-level reranker that operates on the fused candidate set. Rejected for this ADR —
cross-encoder reranking is orthogonal (ADR-061) and applies to single-backend search too. If
added, it composes after the fusion this ADR specifies.

## Consequences

### Positive

- ADR-024's "search all notes" contract holds across multi-backend deployments
- No per-backend weight knob to misconfigure
- Single-backend behavior unchanged (RRF over one list is identity)
- Engine-level tuning (ADR-082 calibration parameters) remains the operator's primary lever
- Substrate-kind dispatch is centralized — pack handlers stay single-backend

### Negative

- Per-query cost scales linearly with the number of backends hosting the substrate kind. For a
  3-backend deployment with note hosting on all three, each `search(kind=note)` triggers three
  per-backend searches in parallel.
- Candidate pool size is now `top_k × multiplier × N_backends` — memory cost of fusion grows
  linearly with backend count

### Neutral

- `khive-fusion` requires no new fusion strategies — `FusionStrategy::Rrf` already exists for
  engine-level fusion; reused for backend-level fusion at a different layer
- Granular-kind verbs see no change — they remain pack-local

## Open Questions

1. **Should `min_score` apply per-backend or post-fusion?** Currently post-fusion (after RRF).
   Per-backend filtering would be cheaper but harder to reason about across heterogeneous
   per-backend pipelines. Defer to operational evidence.
2. **Backend-level filter on the search verb?** A caller might want
   `search(kind=note, backends=["main"])`. The `target_backends: Option<Vec<String>>` field
   supports this; the question is whether to expose it through the public verb surface or keep
   it kernel-internal. v1: kernel-internal only.

## References

- ADR-019 — substrate vs granular kinds (the dispatch discriminator)
- ADR-024 — cross-substrate search contract (the contract this ADR fulfills)
- ADR-025 — pack standard (granular kinds are pack-owned)
- ADR-061 — reranker stage composes after this fusion if/when implemented
- ADR-078 / ADR-082 — engine weights operate within a backend; this ADR explains why no
  per-backend weights
- ADR-079 — backends declared here
- ADR-080 — umbrella
- RuVector `distributed/coordinator.rs` — QueryPlan pattern (explicitly NOT adopted)
