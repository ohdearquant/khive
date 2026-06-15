//! PR2 lifecycle tests: tombstone + Wolverine 2-hop repair (ADR-052 §2).

use std::collections::HashSet;

use khive_vamana::{VamanaConfig, VamanaIndex};
use rand::{prelude::*, SeedableRng};

fn rand_unit_vectors(n: usize, dim: usize, seed: u64) -> Vec<f32> {
    let mut rng = StdRng::seed_from_u64(seed);
    let mut raw: Vec<f32> = (0..n * dim).map(|_| rng.gen_range(-1.0f32..1.0)).collect();
    for row in raw.chunks_mut(dim) {
        let norm: f32 = row.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for x in row.iter_mut() {
                *x /= norm;
            }
        }
    }
    raw
}

fn check_reverse_adj_invariant(index: &VamanaIndex) {
    let g = index.graph();
    let adj = g.adjacency();
    let rev = g.reverse_adjacency();
    for (u, outs) in adj.iter().enumerate() {
        for &v in outs {
            assert!(
                rev[v as usize].contains(&(u as u32)),
                "forward edge {u}→{v} missing from reverse_adj[{v}]"
            );
        }
    }
    for (v, ins) in rev.iter().enumerate() {
        for &u in ins {
            assert!(
                adj[u as usize].contains(&(v as u32)),
                "reverse_adj[{v}] contains {u} but adjacency[{u}] lacks {v}"
            );
        }
    }
}

/// After tombstone(), deleted node has no live in-neighbors (the OQ4 invariant tested hard).
#[test]
fn tombstone_node_has_empty_in_neighbors_post_repair() {
    let n = 50usize;
    let dim = 8usize;
    let vectors = rand_unit_vectors(n, dim, 0xDEAD_0001);
    let cfg = VamanaConfig::with_dimensions(dim)
        .with_max_degree(8)
        .with_search_list_size(16);
    let mut idx = VamanaIndex::build(&vectors, cfg).unwrap();

    // Tombstone node 7 (arbitrary, non-medoid for simplicity).
    let target = if idx.graph().medoid() == 7 {
        8u32
    } else {
        7u32
    };
    idx.tombstone(target).unwrap();

    assert!(
        idx.graph().reverse_adjacency()[target as usize].is_empty(),
        "deleted node {target} must have empty in-neighbors after Wolverine repair"
    );
}

/// reverse_adj bidirectional invariant holds after tombstone().
#[test]
fn reverse_adj_invariant_holds_after_tombstone() {
    let n = 80usize;
    let dim = 8usize;
    let vectors = rand_unit_vectors(n, dim, 0xDEAD_0002);
    let cfg = VamanaConfig::with_dimensions(dim)
        .with_max_degree(8)
        .with_search_list_size(16);
    let mut idx = VamanaIndex::build(&vectors, cfg).unwrap();

    // Tombstone 10 non-medoid nodes.
    let medoid = idx.graph().medoid();
    let mut to_delete: Vec<u32> = (0..n as u32).filter(|&i| i != medoid).take(10).collect();
    for node in to_delete.drain(..) {
        idx.tombstone(node).unwrap();
        check_reverse_adj_invariant(&idx);
    }
}

/// reverse_adj bidirectional invariant holds after tombstone_batch().
#[test]
fn reverse_adj_invariant_holds_after_tombstone_batch() {
    let n = 80usize;
    let dim = 8usize;
    let vectors = rand_unit_vectors(n, dim, 0xDEAD_0003);
    let cfg = VamanaConfig::with_dimensions(dim)
        .with_max_degree(8)
        .with_search_list_size(16);
    let mut idx = VamanaIndex::build(&vectors, cfg).unwrap();

    let medoid = idx.graph().medoid();
    let batch: Vec<u32> = (0..n as u32).filter(|&i| i != medoid).take(15).collect();
    idx.tombstone_batch(&batch).unwrap();
    check_reverse_adj_invariant(&idx);
}

/// tombstone() on the medoid triggers re-election to a live node.
#[test]
fn tombstone_medoid_triggers_re_election() {
    let n = 40usize;
    let dim = 8usize;
    let vectors = rand_unit_vectors(n, dim, 0xDEAD_0004);
    let cfg = VamanaConfig::with_dimensions(dim)
        .with_max_degree(8)
        .with_search_list_size(16);
    let mut idx = VamanaIndex::build(&vectors, cfg).unwrap();

    let old_medoid = idx.graph().medoid();
    idx.tombstone(old_medoid).unwrap();

    let new_medoid = idx.graph().medoid();
    assert_ne!(new_medoid, old_medoid, "medoid must change after tombstone");
    assert!(
        !idx.is_tombstoned(new_medoid),
        "re-elected medoid {new_medoid} must be live"
    );

    // Search must still work from the new medoid.
    let query = rand_unit_vectors(1, dim, 0xDEAD_0099);
    let results = idx.search(&query, 5).unwrap();
    assert!(!results.is_empty());
    for (id, _) in &results {
        assert!(!idx.is_tombstoned(*id), "result node {id} is tombstoned");
    }
}

/// tombstone_batch() with medoid in the batch re-elects exactly once.
#[test]
fn tombstone_batch_with_medoid_re_elects_once() {
    let n = 40usize;
    let dim = 8usize;
    let vectors = rand_unit_vectors(n, dim, 0xDEAD_0005);
    let cfg = VamanaConfig::with_dimensions(dim)
        .with_max_degree(8)
        .with_search_list_size(16);
    let mut idx = VamanaIndex::build(&vectors, cfg).unwrap();

    let medoid = idx.graph().medoid();
    // Batch includes the medoid plus some other nodes.
    let others: Vec<u32> = (0..n as u32).filter(|&i| i != medoid).take(5).collect();
    let mut batch = others.clone();
    batch.push(medoid);

    idx.tombstone_batch(&batch).unwrap();

    let new_medoid = idx.graph().medoid();
    assert!(
        !idx.is_tombstoned(new_medoid),
        "re-elected medoid must be live"
    );
    assert!(
        !batch.contains(&new_medoid),
        "new medoid must not be in the tombstoned batch"
    );
}

/// No tombstoned ordinal appears in search results.
#[test]
fn search_never_returns_tombstoned_nodes() {
    let n = 100usize;
    let dim = 16usize;
    let vectors = rand_unit_vectors(n, dim, 0xDEAD_0006);
    let cfg = VamanaConfig::with_dimensions(dim)
        .with_max_degree(12)
        .with_search_list_size(24);
    let mut idx = VamanaIndex::build(&vectors, cfg).unwrap();

    let medoid = idx.graph().medoid();
    let tombstone_set: HashSet<u32> = (0..n as u32)
        .filter(|&i| i != medoid && i % 5 == 0)
        .collect();
    let batch: Vec<u32> = tombstone_set.iter().copied().collect();
    idx.tombstone_batch(&batch).unwrap();

    let queries = rand_unit_vectors(20, dim, 0xDEAD_0007);
    for qi in 0..20 {
        let q = &queries[qi * dim..(qi + 1) * dim];
        let results = idx.search(q, 10).unwrap();
        for (id, _) in &results {
            assert!(
                !tombstone_set.contains(id),
                "search returned tombstoned node {id}"
            );
        }
    }
}

/// tombstone() rejects out-of-range node.
#[test]
fn tombstone_rejects_out_of_range_node() {
    let n = 10usize;
    let dim = 4usize;
    let vectors = rand_unit_vectors(n, dim, 0xDEAD_0008);
    let cfg = VamanaConfig::with_dimensions(dim)
        .with_max_degree(4)
        .with_search_list_size(8);
    let mut idx = VamanaIndex::build(&vectors, cfg).unwrap();
    assert!(idx.tombstone(999u32).is_err());
}

/// tombstone() rejects double-tombstone.
#[test]
fn tombstone_rejects_already_tombstoned() {
    let n = 10usize;
    let dim = 4usize;
    let vectors = rand_unit_vectors(n, dim, 0xDEAD_0009);
    let cfg = VamanaConfig::with_dimensions(dim)
        .with_max_degree(4)
        .with_search_list_size(8);
    let mut idx = VamanaIndex::build(&vectors, cfg).unwrap();

    let medoid = idx.graph().medoid();
    let target = if medoid == 0 { 1u32 } else { 0u32 };
    idx.tombstone(target).unwrap();
    assert!(idx.tombstone(target).is_err(), "double-tombstone must fail");
}

/// tombstone() on the last live node is rejected atomically — no state mutation on Err.
///
/// Index has n=2. Tombstone node 0, then try to tombstone node 1. The second call
/// would leave zero live nodes. It must return Err AND leave the index intact:
/// - tombstone_count stays at 1 (not 2)
/// - node 1 is not marked tombstoned
/// - search still returns results from node 1
#[test]
fn tombstone_rejects_all_tombstoned_single() {
    let n = 2usize;
    let dim = 4usize;
    let vectors = rand_unit_vectors(n, dim, 0xDEAD_0010);
    let cfg = VamanaConfig::with_dimensions(dim)
        .with_max_degree(4)
        .with_search_list_size(8);
    let mut idx = VamanaIndex::build(&vectors, cfg).unwrap();

    // Tombstone the non-medoid node first.
    let medoid = idx.graph().medoid();
    let first = if medoid == 0 { 1u32 } else { 0u32 };
    idx.tombstone(first).unwrap();
    assert_eq!(idx.tombstone_count(), 1);

    // Now try to tombstone the medoid — would leave zero live nodes.
    let result = idx.tombstone(medoid);
    assert!(
        result.is_err(),
        "tombstone on last live node must return Err"
    );

    // State must be unchanged: count still 1, medoid not tombstoned, search works.
    assert_eq!(
        idx.tombstone_count(),
        1,
        "tombstone_count mutated on Err path"
    );
    assert!(
        !idx.is_tombstoned(medoid),
        "medoid incorrectly marked tombstoned after rejected op"
    );
    let q = &vectors[medoid as usize * dim..(medoid as usize + 1) * dim];
    let results = idx.search(q, 1).unwrap();
    assert!(
        !results.is_empty(),
        "search must still return results after rejected tombstone"
    );
}

/// tombstone_batch() covering all nodes is rejected atomically — no state mutation on Err.
///
/// Index has n=3. Build all-ordinals batch. Call must return Err AND leave the index
/// completely untouched: tombstone_count stays 0, no node is marked tombstoned.
#[test]
fn tombstone_batch_rejects_all_tombstoned() {
    let n = 3usize;
    let dim = 4usize;
    let vectors = rand_unit_vectors(n, dim, 0xDEAD_0011);
    let cfg = VamanaConfig::with_dimensions(dim)
        .with_max_degree(4)
        .with_search_list_size(8);
    let mut idx = VamanaIndex::build(&vectors, cfg).unwrap();

    let all: Vec<u32> = (0..n as u32).collect();
    let result = idx.tombstone_batch(&all);
    assert!(
        result.is_err(),
        "tombstone_batch covering all nodes must return Err"
    );

    // Atomicity: no state mutation.
    assert_eq!(
        idx.tombstone_count(),
        0,
        "tombstone_count mutated on Err path"
    );
    for i in 0..n as u32 {
        assert!(
            !idx.is_tombstoned(i),
            "node {i} incorrectly marked tombstoned after rejected batch"
        );
    }

    // Search still works.
    let q = &vectors[0..dim];
    let results = idx.search(q, 1).unwrap();
    assert!(
        !results.is_empty(),
        "search must still work after rejected tombstone_batch"
    );
}

/// tombstone_batch() rejects a batch containing a duplicate ordinal atomically.
///
/// Duplicate ordinals corrupt tombstone_count (the bit-set op is idempotent but the
/// counter increment is not). The preflight must catch duplicates and return Err before
/// any mutation so tombstone_count stays at 0 and the node remains live.
#[test]
fn tombstone_batch_rejects_duplicate_ordinal() {
    let n = 5usize;
    let dim = 4usize;
    let vectors = rand_unit_vectors(n, dim, 0xDEAD_0012);
    let cfg = VamanaConfig::with_dimensions(dim)
        .with_max_degree(4)
        .with_search_list_size(8);
    let mut idx = VamanaIndex::build(&vectors, cfg).unwrap();

    // Batch with a repeated ordinal — must be rejected.
    let medoid = idx.graph().medoid();
    let target = if medoid == 0 { 1u32 } else { 0u32 };
    let dup_batch = vec![target, target];
    let result = idx.tombstone_batch(&dup_batch);
    assert!(
        result.is_err(),
        "tombstone_batch with duplicate ordinal must return Err"
    );

    // Atomicity: no state mutation.
    assert_eq!(
        idx.tombstone_count(),
        0,
        "tombstone_count mutated on duplicate-ordinal Err path"
    );
    assert!(
        !idx.is_tombstoned(target),
        "node {target} incorrectly marked tombstoned after rejected duplicate batch"
    );

    // Search still works.
    let q = &vectors[0..dim];
    let results = idx.search(q, 1).unwrap();
    assert!(
        !results.is_empty(),
        "search must still work after rejected tombstone_batch"
    );
}

/// tombstone_batch_no_repair() rejects a batch containing a duplicate ordinal atomically.
///
/// Same invariant as tombstone_batch: duplicate ordinals must be caught in preflight
/// before any mutation so tombstone_count stays at 0 and the node remains live.
#[test]
fn tombstone_batch_no_repair_rejects_duplicate_ordinal() {
    let n = 5usize;
    let dim = 4usize;
    let vectors = rand_unit_vectors(n, dim, 0xDEAD_0013);
    let cfg = VamanaConfig::with_dimensions(dim)
        .with_max_degree(4)
        .with_search_list_size(8);
    let mut idx = VamanaIndex::build(&vectors, cfg).unwrap();

    let medoid = idx.graph().medoid();
    let target = if medoid == 0 { 1u32 } else { 0u32 };
    let dup_batch = vec![target, target];
    let result = idx.tombstone_batch_no_repair(&dup_batch);
    assert!(
        result.is_err(),
        "tombstone_batch_no_repair with duplicate ordinal must return Err"
    );

    // Atomicity: no state mutation.
    assert_eq!(
        idx.tombstone_count(),
        0,
        "tombstone_count mutated on duplicate-ordinal Err path"
    );
    assert!(
        !idx.is_tombstoned(target),
        "node {target} incorrectly marked tombstoned after rejected duplicate batch"
    );

    // Search still works.
    let q = &vectors[0..dim];
    let results = idx.search(q, 1).unwrap();
    assert!(
        !results.is_empty(),
        "search must still work after rejected tombstone_batch_no_repair"
    );
}

/// tombstone_count() and is_tombstoned() are accurate.
#[test]
fn tombstone_count_and_is_tombstoned_accurate() {
    let n = 20usize;
    let dim = 4usize;
    let vectors = rand_unit_vectors(n, dim, 0xDEAD_000A);
    let cfg = VamanaConfig::with_dimensions(dim)
        .with_max_degree(4)
        .with_search_list_size(8);
    let mut idx = VamanaIndex::build(&vectors, cfg).unwrap();

    assert_eq!(idx.tombstone_count(), 0);
    let medoid = idx.graph().medoid();
    let target = if medoid == 0 { 1u32 } else { 0u32 };
    idx.tombstone(target).unwrap();
    assert_eq!(idx.tombstone_count(), 1);
    assert!(idx.is_tombstoned(target));
    assert!(!idx.is_tombstoned(medoid));
}

/// needs_consolidation() is false after a few deletes (well below tau).
#[test]
fn needs_consolidation_false_below_tau() {
    let n = 20usize;
    let dim = 4usize;
    let vectors = rand_unit_vectors(n, dim, 0xDEAD_000B);
    let cfg = VamanaConfig::with_dimensions(dim)
        .with_max_degree(4)
        .with_search_list_size(8);
    let mut idx = VamanaIndex::build(&vectors, cfg).unwrap();

    let medoid = idx.graph().medoid();
    let target = if medoid == 0 { 1u32 } else { 0u32 };
    idx.tombstone(target).unwrap();
    assert!(
        !idx.needs_consolidation(),
        "one delete should not trigger consolidation (tau=40000)"
    );
}

/// OQ1 empirical drift test: at 20% deletion on N=1000, Wolverine repair must beat
/// a genuine no-repair control AND stay >= 0.95 * pre-deletion baseline.
///
/// Control construction: `tombstone_batch_no_repair` uses the same graph topology and
/// search config as the repaired index. The ONLY difference is that in-neighbor lists
/// are NOT rewired by RobustPrune — dead-end paths remain in the graph. The search-time
/// tombstone skip (`Option<&[u64]>` in `greedy_search_inner`) applies to BOTH indexes,
/// so any recall delta is attributable solely to Wolverine repair vs. no repair.
///
/// # Literature grounds
/// - FreshDiskANN (SIGMOD 2022): consolidates at 20% deletion, maintains >95% recall.
/// - Wolverine (PVLDB 18(7):2268-2280, VLDB 2025): 2-hop monotonic-path repair on delete.
#[test]
fn oq1_wolverine_repair_beats_no_repair_and_meets_literature_floor() {
    let n = 1000usize;
    let dim = 32usize; // smaller than 384 for CI speed; the repair property is distribution-agnostic
    let k = 10usize;

    let corpus = rand_unit_vectors(n, dim, 0xBEEF_C052);
    // All three indexes use IDENTICAL cfg so any recall delta comes solely from repair.
    let cfg = VamanaConfig::with_dimensions(dim)
        .with_max_degree(16)
        .with_search_list_size(32);

    // Build baseline index and measure pre-deletion recall.
    let baseline_idx = VamanaIndex::build(&corpus, cfg.clone()).unwrap();
    let queries = rand_unit_vectors(50, dim, 0xBEEF_CAFE);
    let baseline = baseline_idx
        .recall_at_k(&queries, k)
        .expect("baseline recall");

    // Select 20% of nodes to delete (none of them the medoid to avoid trivial reconstruction).
    let medoid = baseline_idx.graph().medoid();
    let tombstone_set: Vec<u32> = (0..n as u32)
        .filter(|&i| i != medoid && i % 5 == 0) // every 5th node = 20%
        .collect();
    assert_eq!(tombstone_set.len(), n / 5, "must delete exactly 20%");

    // Build repaired index (Wolverine 2-hop repair via normal tombstone_batch).
    let mut repaired_idx = VamanaIndex::build(&corpus, cfg.clone()).unwrap();
    repaired_idx
        .tombstone_batch(&tombstone_set)
        .expect("repaired tombstone_batch");

    // Build genuine no-repair control: same cfg, same tombstone set, but in-neighbors are
    // NOT rewired by RobustPrune. Dead-end paths to deleted nodes remain; search skips
    // tombstoned nodes at query time. This isolates Wolverine repair as the only variable.
    let mut control_idx = VamanaIndex::build(&corpus, cfg.clone()).unwrap();
    control_idx
        .tombstone_batch_no_repair(&tombstone_set)
        .expect("control tombstone_batch_no_repair");

    let repaired = repaired_idx
        .recall_at_k(&queries, k)
        .expect("repaired recall");
    let control = control_idx
        .recall_at_k(&queries, k)
        .expect("control recall");

    println!("baseline={baseline:.4} control={control:.4} repaired={repaired:.4}");

    // PRIMARY assertion: Wolverine repair must beat no-repair control.
    assert!(
        repaired > control,
        "repair recall {repaired:.4} did not beat no-repair control {control:.4}"
    );

    // SANITY-CHECK: literature target — recall@10 >= 0.95x pre-deletion baseline.
    // Grounded in FreshDiskANN (SIGMOD 2022) which consolidates at 20% deletion and
    // maintains >95% recall, and Wolverine (PVLDB 18(7):2268-2280, VLDB 2025) 2-hop repair.
    assert!(
        repaired >= 0.95 * baseline,
        "repaired recall {repaired:.4} fell below 0.95x baseline {:.4}",
        0.95 * baseline
    );

    // HARD INVARIANT: no tombstoned ordinal in any result set from the repaired index.
    let tombstone_set_hs: HashSet<u32> = tombstone_set.into_iter().collect();
    for qi in 0..50 {
        let q = &queries[qi * dim..(qi + 1) * dim];
        let results = repaired_idx.search(q, k).unwrap();
        for (id, _) in &results {
            assert!(
                !tombstone_set_hs.contains(id),
                "repaired index returned tombstoned node {id} in query {qi}"
            );
        }
    }
}
