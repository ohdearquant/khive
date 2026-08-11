# khive-vamana `index.rs` — Design Notes

`index.rs` glues the build/search algorithm (`graph.rs`), the SQ8 acquisition-tier codec
(`khive_quant`), and on-disk persistence (v1/v2 segments, the ADR-110 portable container,
and `VamanaSnapshot`) into the public `VamanaIndex` type. This file covers design
narrative that used to live as inline comments; see `design.md` for the crate-level
overview, `api/algorithm.md` for the build/search algorithm, and `api/persistence.md`
for the on-disk formats.

## Insert: back-edge and medoid-pin rules

`VamanaIndex::insert` (ADR-052 §2/PR3) must never make a previously-reachable vector
unreachable — the *never-drop insert* invariant. It satisfies this without ever pruning
an existing node's adjacency to make room for a new edge:

- **Back-edge rule (Option E).** After RobustPrune selects the new node's out-neighbors,
  for each selected neighbor `j` the code adds the back-edge `j → ordinal` **only if** `j`
  has a free slot (`|adj(j)| < max_degree`). If `j` is already at `max_degree`, the
  back-edge is skipped entirely — the code does not call `robust_prune_inner(j)` and does
  not drop any of `j`'s existing edges. Pruning `j`'s adjacency to make room was the cause
  of earlier orphan/disconnect defects.
- **Trade-off.** Skipping back-edges on saturated neighbors lowers incremental graph
  quality on heavily-saturated graphs (the new node leans more on the medoid hub for
  routing instead of well-connected back-edges). This is a quality trade-off, not a
  correctness issue — recall stays bounded and is ADR-052-acceptable. A future
  consolidate-side redistribution pass (separate issue + ADR-052 amendment) could repair
  it.
- **Medoid-pin eager repair.** If no selected out-neighbor had a free slot, the inserted
  node has zero inbound edges and would be unreachable. The code pins it by adding
  `medoid → ordinal`; the medoid is the search entry point and is always reachable, so it
  is the designated overflow node for this edge. Native directory and snapshot writers
  cap this overflow to respect the existing degree contract; the ADR-110 portable writer
  preserves it losslessly so byte round-trips retain reachability. (When the graph was
  empty before this insert and `ordinal` became the medoid itself, no pin is needed — that
  is the separate `live_before == 0` branch.)

## Checkpoint-sequence regression guard

`reject_checkpoint_sequence_regression` (called from `save_atomic_with_lock_hook` before
any new segment is staged) rejects a rebuild candidate whose `last_applied_seq` is lower
than the on-disk incumbent's — normally the right thing, since a repair checkpoint should
never regress past a newer commit.

The guard only applies that rejection to a **structurally valid** incumbent
(`validate_v2_structural`). A checksum only proves the bytes on disk match what
`save_atomic` last wrote; it says nothing about whether a writer bug left `reverse_adj`
out of sync with the forward graph, or left node/tombstone counts inconsistent with the
commit record. If the guard treated a checksum-valid-but-structurally-corrupt incumbent as
a legitimate barrier, it would reject every future repair checkpoint below its sequence
forever, even though nothing below that sequence was actually the problem — the
corruption would become permanent. So a structurally invalid incumbent is treated as no
incumbent at all, and the repair checkpoint is allowed to publish.

`validate_v2_structural` and the guard's own inline structural check both replicate every
check `load_v2_fast` applies to a v2 checkpoint (config validation, `read_graph`'s
degree/neighbor/medoid bounds, exact vectors.bin byte-length match, `parse_lifecycle`'s
bounds checks, bidirectional `reverse_adj` consistency via the invariant at
`graph.rs:96-98` — `reverse_adj[v] == { u | v ∈ adjacency[u] }` — and, when present,
`codes.bin`'s magic/shape/finite-codec checks). Replicating the loader's checks here, not
just the checksum, keeps "guard-valid" and "loader-loadable" the same statement; letting
them diverge for any segment (including `codes.bin`, which the guard used to check by hash
alone) reopens the same forever-blocked-repair failure mode for that segment.

## Test fixture notes

- **SQ8 recall parity gate** (`sq8_recall_parity_vs_f32_oracle`, ADR-052 §1 Step 2):
  builds one SQ8-wired index and measures recall@10 for two oracles on the same graph
  topology — `greedy_search_inner` (exact f32) and `greedy_search_inner_sq8` (SQ8
  acquisition + f32 re-score) — against exact brute-force ground truth. Asserts SQ8
  recall is within 0.02 of f32 recall (rounding tolerance) and at or above an absolute
  0.80 floor. Prints both measured values.

- **OOD fallback ranking-flip fixture** (`sq8_ood_fallback_deterministic_ranking_flip`,
  ADR-052 §2): a fixed 10-vector 2-D corpus (`random.Random(seed=0)`) with global-scale
  codec `gs ≈ 0.00287`. The query's dim 0 is `-7.36`, far below the corpus minimum
  (≈0.28), so encoding clamps it to code 0 — `q_enc = [0, 0]`. In code space this makes
  `n1` (encoded `[48, 3]`, SQ8 dist² = 48² + 3² = 2313) look closer than the true nearest
  `n6` (encoded `[0, 176]`, SQ8 dist² = 176² = 30976), even though in exact f32 `n6` is
  genuinely nearest (dist² ≈ 58.8 vs `n1`'s ≈ 60.5). The index is built with a tight
  `search_list_size = max_degree = 4` so traversal commits early. The test asserts (a) the
  SQ8-only path (calling `greedy_search_inner_sq8` directly) picks `n1` — confirming the
  fixture is non-vacuous — and (b) `index.search()`'s OOD-gated f32 fallback picks `n6`,
  matching ground truth. Removing the `is_in_distribution` fallback branch in `search()`
  would make it return `n1` and turn this test red.

- **Equal-code collision test** (`sq8_equal_code_collision_correctness`, ADR-052 §2): a
  1-D corpus `[0.0, 0.001, 0.9]`. A codec trained on `[0.0, 1.0]` in 1-D has
  `gs = 1/255 ≈ 0.00392`, so `v0 = 0.0` and `v1 = 0.001` both round to code 0 — only exact
  f32 can distinguish them. Asserts that SQ8 greedy search (on a 3-node ring graph) and
  SQ8 RobustPrune (candidates `[0, 1]` from node 2, `alpha = 1.0`) return the same result
  as the exact-f32 variants despite the collision.

- **RobustPrune alpha-predicate regression**
  (`sq8_robust_prune_alpha_predicate_collision_regression`, ADR-052 §2): reproduces a bug
  where, when a node and multiple candidates all collapse to the same u8 code, the SQ8
  pool's `d2_node_candidate` is 0, so the strict-≤ prune check
  (`alpha² * dist(selected, candidate) <= 0`) is false for any non-zero inter-selected
  distance and never prunes — even when exact f32 would. Fixture: vectors
  `[0.0, 0.001, 0.0018, 1.0]` (the last anchors the global scale so `gs = 1/255`; the first
  three all encode to code 0), node 0, candidates `[1, 2]`, `alpha = 1.2`. Exact f32
  RobustPrune selects only `v1` (it prunes `v2`: `alpha² * d(v1,v2) ≈ 9.22e-7 ≤
  d(v0,v2) ≈ 3.24e-6`). The fix — using the exact-f32 distance as the predicate's RHS
  instead of the SQ8-pool distance — makes the SQ8 path match. Reverting to the old SQ8
  distance as RHS turns this test red.

- **Checksum-valid-but-structurally-corrupt incumbent tests**
  (`checkpoint_publication_repairs_structurally_corrupt_incumbent`,
  `..._with_malformed_codes_segment`, `..._with_wrong_length_vectors_segment`): each
  builds an incumbent commit at `last_applied_seq = Some(500)`, corrupts one segment in a
  way that still passes its own blake3 checksum after re-signing `metadata.bin` (a phantom
  `reverse_adj` in-neighbor for lifecycle, a bad magic byte for `codes.bin`, a
  one-`f32`-short truncation for `vectors.bin`), asserts `VamanaIndex::load` rejects the
  result as `InvalidFormat`, then asserts a repair checkpoint at a *lower* sequence
  (`Some(100)`) still publishes successfully — proving the corrupt incumbent is not
  treated as a legitimate regression barrier. See "Checkpoint-sequence regression guard"
  above for why this must hold.

- **Allocation-bomb guards**
  (`checkpoint_guard_rejects_absurd_num_vectors_without_huge_allocation`,
  `parse_lifecycle_rejects_short_body_for_absurd_rev_num_nodes_without_huge_allocation`,
  `parse_codes_bin_rejects_overflowing_shape_without_allocation`): a re-signed segment can
  declare an attacker-controlled shape (`num_nodes` near `u32::MAX` in `graph.bin`,
  `rev_num_nodes` in `lifecycle.bin`, or `dims`/`count` values whose header-length
  arithmetic overflows `usize` in `codes.bin`) while the segment's actual bytes stay tiny.
  Before each parser's preflight check (bounding the declared count against the segment's
  actual remaining length, or using checked arithmetic for the header-length computation),
  the corresponding `Vec::with_capacity` call would try to reserve tens of gigabytes and
  abort the process instead of returning `InvalidFormat`. These tests assert the rejection
  is both correct and fast (under 10s, i.e. no huge allocation attempted).

## Concurrency test harness

`save_atomic_publication_serializes_concurrent_writers` verifies that
`save_atomic_with_lock_hook`'s `.checkpoint.lock` handshake genuinely serializes two
concurrent writers rather than merely happening to not race in practice. One writer
(`newer`) takes the lock and blocks (via a channel) until released. The second writer
(`stale`)'s lock-acquisition hook calls `try_lock()` first: an OS-reported `WouldBlock`
result is proof of genuine contention (it can only occur if an incompatible lock is held
at that exact instant), never a timing guess. The probe result is sent over a channel
before the hook falls back to a blocking `lock()`, so the test's rendezvous resolves on
every possible probe outcome and cannot hang on a live probe — the receive timeout only
guards against the hook never running at all (e.g. a panic before `try_lock`).
