# ADR-052: ANN Production Lifecycle -- SQ8 Quantization, Tombstone Delete, Consolidation, Crash-Safe Persistence

**Status**: accepted (2026-06-14)
**Date**: 2026-06-13

## Context

khive ships two ANN index crates. **khive-hnsw** is the mature index: hierarchical navigable
small-world graph, search-time INT8 quantization (`QuantizedArena`), snapshot persistence
(currently behind an optional checkpoint feature gate), and incremental operations.
**khive-vamana** is the DiskANN-style index consumed directly as an ANN bridge
([ADR-030](./ADR-030-retrieval-stack-port.md)). It provides single-shot batch build
(`build()`), greedy search (`search()`), and an L2² SIMD distance kernel.
It has **no delete, no incremental insert, no consolidation, and no acquisition-time
quantization**.

Note on trait boundaries: `VectorStore` ([ADR-005](./ADR-005-storage-capability-traits.md)) is
currently implemented only by `SqliteVecStore` in `khive-db`. khive-vamana is not behind the
`VectorStore` trait; it is consumed directly as a separate ANN bridge
alongside the sqlite-vec path. `VectorStore::insert` and `VectorStore::delete` already exist in
the trait definition ([ADR-005](./ADR-005-storage-capability-traits.md) lines 82-85).

For a corpus that grows and churns over time, khive-vamana is an index, not yet
a fully production-capable database component. Three gaps this ADR proposes to close:

1. **No delete.** The only way to remove a vector from the ANN graph is a full rebuild. A
   long-lived corpus with hard-deleted or orphaned vector rows accumulates dead vectors in the
   ANN graph, and result quality drifts as orphaned content keeps matching. (Superseded records are a
   view-layer concern: they remain in storage and are filtered post-hydration per khive's
   data-vs-view principle. ANN deletion applies to hard-deleted/orphaned vector rows and stale
   snapshots only.)
2. **Cold-start cost on snapshot miss.** The ANN bridge already performs snapshot warm-load first
   (matching fingerprint), falling back to rebuild only when the snapshot is absent, stale, or
   corrupt ([ADR-049](./ADR-049-khived-daemon.md)). On a snapshot hit, restart cost
   is O(deserialize), not O(rebuild). However, the current snapshot format is a JSON/BLOB
   written to `retrieval_snapshots`, which is slow to deserialize at scale (~50-120 s at
   466K vectors). This ADR proposes replacing the JSON/BLOB restore with an mmap/segment
   restore that targets O(load) at the same scale.
3. **No acquisition-time quantization.** Build and search are bottlenecked on f32 distance
   computations. The dominant cost during RobustPrune and greedy search is distance evaluation;
   an integer kernel would cut it substantially.

This ADR proposes a production ANN lifecycle: two-tier SQ8 quantization (new `khive-quant`
crate), tombstone deletion with eager 2-hop repair, periodic consolidation, incremental insert,
and a crash-safe v2 persistence format with fingerprint-gated restore.

## Decision

### 1. Two-tier SQ8 quantization (`khive-quant`, ~800 LOC)

A new Apache-2.0 crate `khive-quant` holds two scalar quantization codecs. The "two-tier"
principle: use **approximate INT8 distances during candidate acquisition** (greedy beam search,
RobustPrune candidate scoring), then **exact f32 distances for final selection** (the returned
top-k, and the edges actually committed during build). The candidate pool is wide enough that
true neighbors survive the approximate filter, so recall is preserved while the bulk of distance
work runs on `u8`.

**`Sq8Codec` -- per-dimension affine (dot / cosine).** Trained over a batch: for each dimension
`i`, `min_i`/`max_i` are the per-dim extrema and `scale_i = (max_i - min_i) / 255`.

```
encode:  code_i = clamp(round((x_i - min_i) / scale_i), 0, 255)   as u8
decode:  x_i ~= min_i + code_i * scale_i
```

Distance is **asymmetric** -- the query stays f32, only the stored vector is quantized:

```
q . v ~= sum_i q_i * (min_i + code_i * scale_i)
       = (sum_i q_i * min_i)  +  sum_i (q_i * scale_i) * code_i
```

The first term and the per-dim weights `q_i * scale_i` are precomputed once per query; the hot
loop is an f32-weight x u8-code dot product. Cosine reuses the dot kernel over L2-normalized
inputs.

**`GsSq8Codec` -- global-scale (L2, Vamana acquisition).** One shared offset `m` and one shared
scale `gs = (global_max - global_min) / 255` across all dimensions. The shared offset is what
makes the integer L2 kernel **algebraically exact in code space** (the offset cancels in the
difference):

```
a_i - b_i = gs * (code_a_i - code_b_i)
||a - b||^2 = sum_i (a_i - b_i)^2 = gs^2 * sum_i (code_a_i - code_b_i)^2
```

The inner sum is integer: each squared code difference is <= 255^2, and the accumulated sum over
384 dims fits comfortably in `u32`. There is no distance-computation error beyond the one-time
encode rounding -- the squared distance is computed exactly on the codes, then scaled by `gs^2`.

**SIMD.** NEON widening intrinsics (`vsubl_u8` -> i16 diffs, `vmull`/`vmlal` for squared
accumulation) with four `u32` accumulators for instruction-level parallelism; portable scalar
fallback for non-NEON targets. Measured kernel cost at 384d on Apple Silicon: `gs_l2_sq` ~12.8 ns
vs f32 dot ~214 ns (~16.8x on the distance kernel alone).

**Where each codec applies:**

- **Vamana**: `GsSq8Codec` for the L2 acquisition tier, default-on (the integer L2 kernel is
  exact in code space, so the build/search speedup carries no recall cost). End-to-end batch
  build improves ~1.27-1.41x (build is distance-bound).
- **HNSW**: `build_batch_sq8()` + `search_layer_sq8()` as an **opt-in** path. The cosine metric
  needs a residual-correction pass that consumes most of the kernel gain, so HNSW SQ8 stays off
  by default and is enabled only where a probe shows net benefit on the target corpus.

Per the perf discipline, `khive-quant` ships with criterion benches that **assert** the kernel
bars, and SQ8-default-on for Vamana lands only after a probe confirms no recall regression at the
scale that matters.

### 2. Vamana lifecycle (tombstone, repair, consolidation, insert)

Five new fields on the Vamana index:

```rust
tombstones: BitVec,             // soft-delete marks, one bit per node
tombstone_count: usize,         // live count of tombstoned nodes
ops_since_consolidation: usize, // delete+insert churn since last compaction
free_slots: Vec<u32>,           // recyclable ordinals from consolidation/delete
reverse_adj: Vec<Vec<u32>>,     // in-neighbors, required for delete repair
```

**`tombstone(node_id)` with eager 2-hop repair (Wolverine, VLDB 2025).** A naive tombstone
leaves the deleted node's in-neighbors pointing at a dead node, degrading navigability. Eager
2-hop repair rewires the neighborhood at delete time so the graph stays navigable without ever
visiting the dead node:

1. Mark `node_id` tombstoned; increment `tombstone_count`, `ops_since_consolidation`.
2. Collect `in = reverse_adj[node_id]` (in-neighbors) and `out = adjacency[node_id]`
   (out-neighbors).
3. For each in-neighbor `p` (skipping tombstoned `p`):
4. candidate pool = `out` ∪ (`adjacency[p]` \ {`node_id`}).
5. drop tombstoned entries from the pool.
6. `new_neighbors = RobustPrune(p, pool, alpha, max_degree)` -- the same prune used at build.
7. replace `adjacency[p]` with `new_neighbors`.
8. update `reverse_adj` for every node whose in-set changed.
9. if `node_id` was the medoid (entry point), re-elect: compute the centroid of live vectors
   and pick the live node nearest the centroid.
10. release `node_id`'s ordinal to `free_slots` (vector bytes reclaimed at consolidation).

Repair is local (the deleted node's 2-hop neighborhood), so per-delete cost is bounded by degree,
not corpus size. Because repair happens at delete time, **recall stays bounded between
consolidations** -- consolidation is then a pure compaction, not a recall-recovery step.

**`insert(vector) -> Result<u32>` -- incremental.** Preflight checks (dimension, finite,
capacity) run before any state mutation; a rejected insert never touches the Mmap store.
`ensure_owned()` then promotes `VectorStorage::Mmap` to `VectorStorage::Owned` (one-time
copy; subsequent inserts pay no promotion cost). Recycles a `free_slots` ordinal if
available (LIFO; double-use guard: slot must be tombstoned before popping), else appends.
Greedy-search finds an entry neighborhood; `RobustPrune` selects the new node's out-edges.

**Never-drop back-edge rule.** For each selected out-neighbor `j`, the back-edge `j→ordinal`
is added ONLY IF `j` has a free slot (`|adj(j)| < max_degree`). If `j` is full, the
back-edge is skipped entirely -- `RobustPrune` is NOT called on `j` and none of `j`'s
existing edges are removed. Correctness invariant: insert() never removes any existing
node's inbound edge, so every node reachable before the insert remains reachable after it.

**Medoid-pin eager repair (insert-time analog of Wolverine for delete).** If, after the
back-edge loop, the inserted node has zero inbound edges (all selected neighbors were full),
it is pinned by adding the edge `medoid→ordinal`. The medoid is the search entry point and
is always reachable; it is the designated overflow node (the same role it plays in DiskANN).
The medoid is permitted to exceed `max_degree` in the live in-memory index (by one edge per
orphan-pinned insert; K consecutive inserts into a saturated graph can accumulate K overflow
edges). No existing medoid edge is removed, so the never-drop invariant is preserved.

**Save/load + snapshot round-trip invariant.** `save()` and `to_snapshot()` cap the medoid's
adjacency to `max_degree` before writing. Any overflow edges beyond `max_degree` are dropped
at serialization time. The result is a written graph
that satisfies all v1 loader degree constraints (`degree ≤ max_degree` everywhere). After
load, recently inserted nodes whose overflow edges were dropped may lack medoid in-edges
and may not be immediately searchable. Today's `consolidate()` handles tombstone compaction
only (renumbering live nodes); it does not redistribute forward edges or re-run RobustPrune
for under-connected nodes. A future redistribution pass (separate issue) will address
post-load reachability recovery; until then, serialization truncation of overflow edges is
a permanent quality reduction for affected nodes absent a full index rebuild. Tests
verify that the post-insert `save()`/`load()` and `to_snapshot()`/`from_snapshot()` round-trips
succeed without degree-violation errors, and that existing nodes remain findable.

**Quality trade-off.** Skipping back-edges on saturated neighbors lowers incremental-insert
graph quality on heavily-saturated corpora (ordinal becomes less well-connected via
back-edges, increasing reliance on the medoid hub for routing). This is a quality trade-off,
not a correctness issue (recall is bounded and ADR-052-acceptable). A future
consolidate-side redistribution pass (separate issue + ADR-052 amendment) will repair the
degree imbalance if bench measurements show it is material.

**Ordinal stability invariant:** ordinals returned by `insert()` are stable until the next
`consolidate()` call. After consolidation the ordinal space is renumbered; callers that
maintain external id→ordinal tables **must** apply the returned remap (see below).

**`consolidate() -> Result<Vec<u32>>` -- compaction with ordinal remapping**, triggered when
`ops_since_consolidation >= tau` (default `tau = 40_000`):

0. Fast-path: if `tombstone_count == 0`, reset `ops_since_consolidation` and return
   `Ok(Vec::new())`. An empty return signals "ordinals unchanged; no remap needed."
1. Build `old -> new` ordinal remap assigning live nodes contiguous ordinals `0..M`.
2. Allocate a fresh vector store of size `M`; copy live vectors to new ordinals.
3. Rewrite every adjacency list through the remap, dropping any tombstoned targets.
4. Rebuild `reverse_adj` from the compacted forward graph.
5. Remap the medoid.
6. Clear `tombstones`, `free_slots`; reset `tombstone_count` and `ops_since_consolidation`.
7. Return `Ok(new_to_old)` where `new_to_old[new_ordinal] = old_ordinal`.

Callers that maintain external id→ordinal mappings
must invert the returned `new_to_old` slice to rebuild their table; a non-empty return is
an epoch boundary. `ensure_owned()` is called at the top of `consolidate()` for the same
Mmap-promotion reason as `insert()`.

Consolidation does **not** re-run graph construction -- the Wolverine repairs already kept the
graph navigable. It reclaims space and restores dense ordinals. (FreshDiskANN / SPFresh use the
same split: cheap eager repair on the hot path, periodic compaction off it.)

### 3. Crash-safe v2 persistence with fingerprint-gated restore

A v2 on-disk format (`KHVVAMG2` magic) replaces a full restart rebuild with an mmap load, and
persists the lifecycle state (tombstones, free slots, reverse adjacency) that v1 does not carry.

**Crash consistency via a commit record.** `save_atomic` writes the bulk segments (vectors,
adjacency) first, then writes `metadata.bin` **last**, carrying blake3 checksums of each segment.
`metadata.bin` is the commit record: if a crash interrupts the save, the previous snapshot's
metadata is still the valid one, so load never observes a torn write. Load verifies every segment
checksum before use.

**`load_or_build` gated by `CorpusFingerprint`:**

```rust
struct CorpusFingerprint {
    vector_count: usize,
    dimensions: usize,
    content_hash: [u8; 32],   // blake3 over the canonical vector bytes
}
```

On open, if the persisted fingerprint matches the live corpus, mmap-restore in O(N); otherwise
rebuild. This turns daemon restart from a rebuild into a load -- expected >=100x at 100K scale
(O(N) mmap vs the many greedy searches of batch construction), to be confirmed by probe before
the claim is made load-bearing.

### Adaptation for khive

- `khive-quant` is a new Apache-2.0 crate in the storage stack
  (`types -> score -> quant -> hnsw/vamana`). It depends only on `khive-types`.
- khive-hnsw keeps its existing snapshot path; the SQ8 build/search functions are additive and
  opt-in. The checkpoint feature gate is orthogonal to this ADR.
- khive-vamana's lifecycle additions (tombstone, insert, consolidate, v2 persistence) are
  internal to the index crate. The ANN bridge gains tombstone/insert/consolidate calls as the
  index-internal interface
  grows. No change to the `VectorStore` trait is proposed here: insert and delete already exist
  on that trait ([ADR-005](./ADR-005-storage-capability-traits.md)), and khive-vamana is not
  behind it.
- `khive-fold` is unaffected -- persistence here is self-contained in the index crate's v2
  format, not routed through fold.

## Migration path

1. Add `khive-quant` (codecs + SIMD dispatch + asserting benches).
2. Wire `GsSq8Codec` into khive-vamana build/search as the default acquisition tier; verify
   recall parity by probe.
3. Add the five lifecycle fields + `tombstone`/`insert`/`consolidate` to khive-vamana with
   isomorphic tests (delete-then-search recall, churn-then-consolidate identity, insert recall).
   `insert` returns the assigned `u32` ordinal; `consolidate` returns `Result<Vec<u32>>`
   (the `new_to_old` remap, or an empty `Vec` when no-op). `ensure_owned()` is called at
   the top of both mutating functions to promote Mmap storage before any write. Ordinals
   are NOT stable across a non-no-op `consolidate`. (Implemented: PR3.)
4. Add the v2 format + `save_atomic` + `load_or_build`; verify crash-consistency by a
   kill-during-save probe and restore-correctness by a fingerprint-match probe.
5. Add `build_batch_sq8`/`search_layer_sq8` to khive-hnsw as an opt-in path; enable per-corpus
   only where a probe shows net benefit.

## Consequences

- khive-vamana snapshot restore changes from JSON/BLOB deserialization to mmap segment load --
  the daemon's snapshot-hit cold path drops from slow deserialization to O(load). Full rebuild
  still occurs on snapshot miss (absent, stale, or corrupt snapshot), as today.
- Delete becomes a real operation (tombstone + eager repair + periodic consolidation) with
  bounded recall drift, instead of "rebuild the whole index."
- Distance-bound build/search speeds up via the integer kernel (~16.8x on the kernel,
  ~1.3-1.4x end-to-end build for Vamana); memory traffic for distance comparisons drops ~4x
  (`u8` vs f32).
- `khive-quant` adds ~800 LOC; the Vamana lifecycle adds the bulk of new code and tests.
- New default-on behavior (Vamana SQ8) is gated on measured recall parity at scale; HNSW SQ8
  stays opt-in by design (cosine residual pass eats the gain).
- Crash safety becomes a tested property (commit-record save + checksum-verified load), not an
  assumption.
