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

The two passes use `alpha = 1.0` then `alpha = config.alpha`:

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
