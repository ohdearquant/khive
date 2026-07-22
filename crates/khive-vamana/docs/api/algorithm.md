# Vamana Algorithm

**Scope:** `crates/khive-vamana/src/graph.rs` — `VamanaGraph::build`, `greedy_search_inner`, `robust_prune_inner`
**ADR refs:** ADR-048 (Vamana as the knowledge-pack ANN engine)
**Last reviewed:** 2026-06-06

---

## Overview

Vamana is a graph-based approximate nearest-neighbor (ANN) index introduced in
the DiskANN paper (Subramanya et al., 2019). `khive-vamana` implements the
in-memory variant with deterministic seeding for reproducibility.

---

## Build phases

### Phase 1 — Random initialization

`initial_random_adjacency` assigns each node `i` a random neighbor list of
size `min(max_degree, N-1)` by a partial Fisher-Yates shuffle over a pool
that excludes `i`. Self-loops and duplicates are impossible by construction.

### Phase 2 — Two-pass refinement

The two passes use `alpha = 1.0` then `alpha = config.alpha`, unconditionally
— including when `config.alpha == 1.0`, so both passes run at the same alpha:

```
for pass_alpha in [1.0, config.alpha]:
    shuffle insertion order
    for batch in shuffled_nodes.chunks(BUILD_BATCH_SIZE):
        snapshot = adjacency.clone()           // read-stable snapshot for parallelism
        proposals = batch.par_iter().map(|node|:
            greedy_search(node) over snapshot
            candidates = expanded ∪ results ∪ current_neighbors
            robust_prune(node, candidates, pass_alpha, max_degree)
        adjacency[proposals] ← proposals      // apply forward edges
        backedges = reverse(proposals)
        adjacency.par_iter_mut().for_each(|target|:
            merge backedges into neighbors
            if overflow: robust_prune(target, merged, pass_alpha, max_degree)
```

Each pass applies forward edges (greedy-search proposals) and back-edges
(connectivity from all nodes that just pointed to a target).

The second pass at `alpha = config.alpha == 1.0` is **not** a redundant rerun
of the first pass, even though `pass_alpha` is identical across both. Each
pass's `greedy_search` runs over the adjacency the *previous* pass left
behind, so the second pass explores a more-connected graph and its
`robust_prune` calls see a different (typically richer) candidate set than
the first pass did — the two passes are not idempotent. Differential testing
at production scale (n>=250, 384-dim normalized vectors) shows the two-pass
adjacency diverging from a single alpha=1.0 pass, for both `build` and
`build_sq8`
(`build_alpha_one_two_passes_are_not_idempotent_at_scale` and
`build_sq8_alpha_one_two_passes_are_not_idempotent_at_scale` in `graph.rs`);
an earlier revision skipped the second pass on the assumption that it was a
no-op at `config.alpha == 1.0` and was reverted once this was disproven.

---

## Greedy search

`greedy_search_inner` maintains a candidate frontier (`Vec<Candidate>`) sorted
by distance. At each step the unexpanded nearest candidate is expanded; its
neighbors enter the frontier. The frontier is bounded to `max(k, search_list_size)`
to cap memory. A generation-based `VisitedSet` avoids revisiting nodes in O(1)
per query.

Result set: top-k candidates from the frontier, sorted by distance, ties broken
by node ID for determinism.

---

## Robust prune

`robust_prune_inner` implements the DiskANN alpha-occlusion heuristic:

```
sort candidates by distance to v (ascending)
for each candidate c (in order):
    if any already-selected neighbor p satisfies alpha^2 * d(p, c)^2 <= d(v, c)^2:
        skip c (p "occludes" c from v's perspective)
    else:
        select c
    stop when |selected| == max_degree
```

The alpha-squared form avoids a `sqrt` per distance pair. At pass `alpha = 1.0`
the graph converges to near-Delaunay; at `alpha = config.alpha` (default 1.2)
the occlusion radius is widened for better long-range connectivity.

---

## Medoid selection

`select_medoid` computes a sample mean over `min(1000, N)` randomly chosen
vectors and returns the corpus vector closest to that mean. This is O(N) and
provides a well-connected starting node for search.

---

## Invariants

- No self-loops. Enforced by `initial_random_adjacency` and snapshot validation.
- No duplicate neighbors. Enforced by `sort_dedup_u32` after pruning and in `from_snapshot`.
- Degree bound: `adjacency[i].len() <= max_degree` after build and after loading.
- Deterministic output: seeded RNG (`BUILD_SEED`) + deterministic sort order ensures identical graphs for identical inputs.

---

## Reverse-adjacency rebuild

`VamanaGraph::rebuild_reverse_adj_from_adjacency` (`graph.rs`) rebuilds `reverse_adj`
from scratch by scanning the current `adjacency`: O(N × R) where N is node count and
R is average out-degree. Called after `build()` completes, and after `load` /
`from_snapshot` restores adjacency from disk — the v1 on-disk format does not persist
`reverse_adj`, so it must be reconstructed rather than loaded.

## Tombstone defense-in-depth

`greedy_search_inner` and `greedy_search_inner_sq8` (`graph.rs`) both take an optional
`tombstones` bitvec. Under a correct Wolverine repair, no live forward edge should ever
point at a tombstoned node — but this guard exists to catch crash-truncated repairs in
the window before PR4 makes the invariant crash-safe:

- If the medoid seed itself is tombstoned, the search returns an empty result rather
  than surfacing a deleted node.
- Neighbors are skipped during beam expansion if tombstoned.
- The final result set is filtered against tombstones before `take(k)`.

## SQ8 acquisition tier

`VamanaGraph::build_sq8`, `greedy_search_inner_sq8`, and `robust_prune_inner_sq8`
(ADR-052 §1, Step 2 — two-tier principle) route graph construction and search through
`GsSq8Codec::l2_sq` (integer L2²) on pre-encoded corpus vectors instead of the f32
kernel, for the frontier priority queue / candidate acquisition stage only:

- `build_sq8` produces a graph topology equivalent to `build`; the caller trains the
  codec and encodes the corpus before calling it.
- `greedy_search_inner_sq8` re-scores every frontier candidate with exact f32 L2²
  before final top-k selection (SQ8 for acquisition, exact f32 for final ranking).
  Frontier ties on equal SQ8 codes are broken with exact f32 distance, since distinct
  f32 vectors can map to the same u8 code (clamped or low-resolution dimensions).
- `robust_prune_inner_sq8` scores and orders candidates with SQ8, tiebreaking equal
  codes with exact f32 to match the search ordering — but the alpha diversity predicate
  itself always uses exact f32 distances on both sides (node→candidate and
  selected→candidate). This is deliberate: if node and candidate collide in code space
  (e.g. both map to code 0), the SQ8 distance is 0, which would make
  `alpha² * dist(selected, candidate) <= 0` vacuously true and incorrectly prune
  candidates that exact f32 would keep. Selected neighbor IDs match the f32 variant on
  the collision cases covered by tests; callers that need exact distances re-score
  after prune.

## Wolverine 2-hop repair

`wolverine_repair` (`index.rs`, ADR-052 §2 steps 3-8) is the core soft-delete repair
step, called from `VamanaIndex::tombstone` and `tombstone_batch`. For each live
in-neighbor `p` of the deleted node, it rewires `p`'s adjacency by running RobustPrune
over the union of the deleted node's out-neighbors and `p`'s current neighbors (minus
the deleted node), with all tombstoned candidates excluded — this preserves
monotonic-path reachability through `p`. `reverse_adj` is updated in lockstep on every
rewire (the PR1 invariant).

References:
- Wolverine: PVLDB 18(7):2268-2280, VLDB 2025 (Liu/Zheng/Yue/Ruan/Zhou/Jensen)
- FreshDiskANN: SIGMOD 2022 (>95% recall at 20% deletion with eager repair)
