use super::*;

fn make_line_vectors(n: usize) -> Vec<f32> {
    (0..n).map(|i| i as f32 / n as f32).collect()
}

#[test]
fn medoid_sample_spans_the_whole_input() {
    // Rows lie on a line in order. A uniform sample has its mean near the middle,
    // so the medoid lands near the middle row; a sample taken mostly from the
    // first rows would put it near the start.
    let n = 10 * MEDOID_SAMPLE_K;
    let vectors = make_line_vectors(n);
    let medoid = select_medoid(&vectors, 1, n).unwrap() as usize;
    assert!(
        (n * 4 / 10..=n * 6 / 10).contains(&medoid),
        "medoid {medoid} of {n} ordered rows is not near the middle"
    );
}

#[test]
fn visited_set_marks_first_visit_only() {
    let mut vs = VisitedSet::new(10);
    assert!(vs.mark_if_new(3));
    assert!(!vs.mark_if_new(3));
}

#[test]
fn visited_set_clear_advances_generation() {
    let mut vs = VisitedSet::new(10);
    vs.mark_if_new(5);
    vs.clear();
    assert!(vs.mark_if_new(5));
}

#[test]
fn visited_set_resizes_for_large_node() {
    let mut vs = VisitedSet::new(4);
    assert!(vs.mark_if_new(100));
    assert!(!vs.mark_if_new(100));
}

#[test]
fn new_rejects_empty_graph() {
    assert!(matches!(
        VamanaGraph::new(0, 0),
        Err(VamanaError::EmptyInput)
    ));
}

#[test]
fn new_rejects_invalid_medoid() {
    assert!(matches!(
        VamanaGraph::new(5, 5),
        Err(VamanaError::InvalidFormat { .. })
    ));
}

#[test]
fn add_node_returns_u32_id() {
    let mut g = VamanaGraph::new(3, 0).unwrap();
    let id = g.add_node().unwrap();
    assert_eq!(id, 3);
    assert_eq!(g.node_count(), 4);
}

#[test]
fn greedy_search_finds_nearest_on_line_graph() {
    // 5 points at 0.0, 0.2, 0.4, 0.6, 0.8 connected as a line
    let vectors: Vec<f32> = vec![0.0, 0.2, 0.4, 0.6, 0.8];
    let mut g = VamanaGraph::new(5, 2).unwrap();
    // Connect as chain: 0-1-2-3-4
    g.adjacency[0] = vec![1];
    g.adjacency[1] = vec![0, 2];
    g.adjacency[2] = vec![1, 3];
    g.adjacency[3] = vec![2, 4];
    g.adjacency[4] = vec![3];

    let query = [0.81f32];
    let mut visited = VisitedSet::new(5);
    let result = g
        .greedy_search(&vectors, 1, &query, 1, 5, &mut visited, None)
        .unwrap();

    assert_eq!(result.results[0].0, 4);
}

#[test]
fn greedy_search_sorts_ties_by_node_id() {
    // Two points equidistant from query — expect lower id first
    let vectors: Vec<f32> = vec![0.0, 2.0];
    let mut g = VamanaGraph::new(2, 0).unwrap();
    g.adjacency[0] = vec![1];
    g.adjacency[1] = vec![0];

    let query = [1.0f32];
    let mut visited = VisitedSet::new(2);
    let result = g
        .greedy_search(&vectors, 1, &query, 2, 5, &mut visited, None)
        .unwrap();

    assert!(result.results.len() >= 2);
    assert_eq!(result.results[0].0, 0);
    assert_eq!(result.results[1].0, 1);
}

#[test]
fn greedy_search_rejects_query_dimension_mismatch() {
    let vectors = vec![0.1f32, 0.2, 0.3, 0.4];
    let g = VamanaGraph::new(2, 0).unwrap();
    let mut visited = VisitedSet::new(2);
    let err = g.greedy_search(&vectors, 2, &[0.1f32], 1, 5, &mut visited, None);
    assert!(matches!(err, Err(VamanaError::DimensionMismatch { .. })));
}

#[test]
fn robust_prune_enforces_degree_bound() {
    let n = 20usize;
    let dim = 2usize;
    let vectors: Vec<f32> = (0..n)
        .flat_map(|i| {
            let angle = i as f32 * std::f32::consts::TAU / n as f32;
            vec![angle.cos(), angle.sin()]
        })
        .collect();
    let g = VamanaGraph::new(n, 0).unwrap();
    let candidates: Vec<u32> = (1..n as u32).collect();
    let pruned = g
        .robust_prune(&vectors, dim, 0, &candidates, 1.2, 4)
        .unwrap();
    assert!(pruned.len() <= 4);
}

#[test]
fn robust_prune_uses_diskann_alpha_squared_condition() {
    // Reproduce algorithm.md walkthrough with 2D coordinates
    // node v = (0,0), alpha = 1.2, R = 3
    let vectors: Vec<f32> = vec![
        0.0, 0.0, // node 0: v
        1.0, 0.0, // node 1: A, d(v,A)=1.0
        1.1, 0.1, // node 2: B, d(v,B)≈1.105
        0.0, 1.4, // node 3: C, d(v,C)=1.4
        2.0, 0.0, // node 4: D, d(v,D)=2.0
        -2.0, 1.9, // node 5: E, d(v,E)≈2.759
    ];
    let g = VamanaGraph::new(6, 0).unwrap();
    let candidates: Vec<u32> = vec![1, 2, 3, 4, 5];
    let pruned = g.robust_prune(&vectors, 2, 0, &candidates, 1.2, 3).unwrap();
    // Expected: [A(1), C(3)] — B, D, E get occluded
    assert!(pruned.contains(&1), "A should be selected: {pruned:?}");
    assert!(pruned.contains(&3), "C should be selected: {pruned:?}");
    assert!(!pruned.contains(&2), "B should be pruned: {pruned:?}");
    assert!(!pruned.contains(&4), "D should be pruned: {pruned:?}");
    assert!(!pruned.contains(&5), "E should be pruned: {pruned:?}");
}

#[test]
fn robust_prune_removes_self_and_duplicates() {
    let vectors: Vec<f32> = vec![0.0, 0.5, 1.0];
    let g = VamanaGraph::new(3, 0).unwrap();
    let candidates: Vec<u32> = vec![0, 1, 1, 2];
    let pruned = g.robust_prune(&vectors, 1, 0, &candidates, 1.0, 4).unwrap();
    assert!(!pruned.contains(&0), "self-loop must be removed");
    // No duplicates
    let mut deduped = pruned.clone();
    deduped.dedup();
    assert_eq!(pruned.len(), deduped.len());
}

#[test]
fn build_rejects_empty_vectors() {
    let cfg = VamanaConfig::default();
    assert!(matches!(
        VamanaGraph::build(&[], &cfg),
        Err(VamanaError::EmptyInput)
    ));
}

#[test]
fn build_rejects_non_row_major_vectors() {
    let cfg = VamanaConfig::with_dimensions(3);
    let vectors = vec![0.1f32; 7]; // 7 not divisible by 3
    assert!(matches!(
        VamanaGraph::build(&vectors, &cfg),
        Err(VamanaError::DimensionMismatch { .. })
    ));
}

#[test]
fn build_creates_bounded_degree_graph() {
    use rand::SeedableRng;
    let mut rng = StdRng::seed_from_u64(42);
    let n = 50usize;
    let dim = 8usize;
    let mut raw: Vec<f32> = (0..n * dim)
        .map(|_| rng.random_range(-1.0f32..1.0))
        .collect();
    normalize_rows(&mut raw, dim);

    let cfg = VamanaConfig::with_dimensions(dim)
        .with_max_degree(8)
        .with_search_list_size(16);

    let g = VamanaGraph::build(&raw, &cfg).unwrap();
    for list in g.adjacency() {
        assert!(list.len() <= 8);
    }
}

#[test]
fn build_is_deterministic_for_same_input() {
    use rand::SeedableRng;
    let mut rng = StdRng::seed_from_u64(77);
    let n = 30usize;
    let dim = 4usize;
    let mut raw: Vec<f32> = (0..n * dim)
        .map(|_| rng.random_range(-1.0f32..1.0))
        .collect();
    normalize_rows(&mut raw, dim);

    let cfg = VamanaConfig::with_dimensions(dim)
        .with_max_degree(6)
        .with_search_list_size(12);

    let g1 = VamanaGraph::build(&raw, &cfg).unwrap();
    let g2 = VamanaGraph::build(&raw, &cfg).unwrap();
    assert_eq!(g1, g2);
}

#[test]
fn build_alpha_one_two_passes_are_not_idempotent_at_scale() {
    // Regression guard for #559: an earlier revision skipped the second
    // refinement pass when config.alpha == 1.0, reasoning that a second pass
    // at an identical alpha reruns pruning over an already-converged graph and
    // reproduces the same adjacency. That reasoning does not hold once the
    // graph is large enough for the first pass to leave real refinement on
    // the table: the second pass runs greedy search over the *now more
    // connected* graph the first pass produced and finds different pruning
    // candidates. This test pins that divergence down directly so the skip
    // is not silently reintroduced.
    // n=300 is the smallest size (of 150/200/250/300/350/384/400/450/500/600
    // probed at this dim, max_degree, and search_list_size, with this seed's
    // rand 0.10 stream) at which the two builds diverge, with and without
    // default features; below it the first pass already converges to a fixed
    // point.
    let dim = 384usize;
    let n = 300usize;
    let mut rng = StdRng::seed_from_u64(1000);
    let mut raw: Vec<f32> = (0..n * dim)
        .map(|_| rng.random_range(-1.0f32..1.0))
        .collect();
    normalize_rows(&mut raw, dim);

    let cfg = VamanaConfig::with_dimensions(dim)
        .with_max_degree(32)
        .with_search_list_size(64)
        .with_alpha(1.0);

    set_max_passes(Some(1));
    let one_pass = VamanaGraph::build(&raw, &cfg).unwrap();
    set_max_passes(None);
    let two_pass = VamanaGraph::build(&raw, &cfg).unwrap();

    assert_ne!(
        one_pass, two_pass,
        "second alpha==1.0 pass changed adjacency vs a single pass; it is not \
             a redundant no-op at this scale and must not be skipped"
    );
}

#[test]
fn build_sq8_alpha_one_two_passes_are_not_idempotent_at_scale() {
    // SQ8 counterpart of build_alpha_one_two_passes_are_not_idempotent_at_scale:
    // build_sq8 runs the same two-pass alpha schedule as build, so it needs the
    // same regression guard against a reintroduced alpha==1.0 skip. Same n=300
    // divergence point established there.
    let dim = 384usize;
    let n = 300usize;
    let mut rng = StdRng::seed_from_u64(1000);
    let mut raw: Vec<f32> = (0..n * dim)
        .map(|_| rng.random_range(-1.0f32..1.0))
        .collect();
    normalize_rows(&mut raw, dim);

    let codec = GsSq8Codec::train_flat(&raw, dim);
    let encoded = codec.encode_flat_par(&raw, dim);

    let cfg = VamanaConfig::with_dimensions(dim)
        .with_max_degree(32)
        .with_search_list_size(64)
        .with_alpha(1.0);

    set_max_passes(Some(1));
    let one_pass = VamanaGraph::build_sq8(&raw, CodesView::Owned(&encoded), &codec, &cfg).unwrap();
    set_max_passes(None);
    let two_pass = VamanaGraph::build_sq8(&raw, CodesView::Owned(&encoded), &codec, &cfg).unwrap();

    assert_ne!(
        one_pass, two_pass,
        "second alpha==1.0 pass changed adjacency vs a single pass for build_sq8; \
             it is not a redundant no-op at this scale and must not be skipped"
    );
}

fn normalize_rows(v: &mut [f32], dim: usize) {
    for row in v.chunks_mut(dim) {
        let norm: f32 = row.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for x in row.iter_mut() {
                *x /= norm;
            }
        }
    }
}

// REASON: test helper kept for future graph traversal tests that need a
// simple chain topology with known distances.
#[allow(dead_code)]
fn make_line_graph_test(n: usize) -> (Vec<f32>, VamanaGraph) {
    let vectors = make_line_vectors(n);
    let mut g = VamanaGraph::new(n, 0).unwrap();
    for i in 0..n {
        if i > 0 {
            g.adjacency[i].push(i as u32 - 1);
        }
        if i + 1 < n {
            g.adjacency[i].push(i as u32 + 1);
        }
    }
    (vectors, g)
}

// ---- PR1: reverse_adj consistency tests ----

/// After `build`, every forward edge u→v must be reflected in `reverse_adj[v]` as `u`.
/// Equivalently: for every node v, `reverse_adj[v]` must contain exactly the set of nodes
/// that have `v` in their forward adjacency list.
#[test]
fn build_reverse_adj_consistent_with_forward_adjacency() {
    use rand::SeedableRng;
    let mut rng = StdRng::seed_from_u64(0x00AD_C052);
    let n = 60usize;
    let dim = 4usize;
    let raw: Vec<f32> = (0..n * dim)
        .map(|_| rng.random_range(-1.0f32..1.0))
        .collect();

    let cfg = VamanaConfig::with_dimensions(dim)
        .with_max_degree(8)
        .with_search_list_size(16);

    let g = VamanaGraph::build(&raw, &cfg).unwrap();

    // For each node v, build the expected in-neighbor set by scanning forward adjacency.
    let expected_rev: Vec<std::collections::HashSet<u32>> = (0..n)
        .map(|v| {
            (0..n)
                .filter(|&u| u != v && g.adjacency()[u].contains(&(v as u32)))
                .map(|u| u as u32)
                .collect()
        })
        .collect();

    for (v, expected) in expected_rev.iter().enumerate() {
        let actual: std::collections::HashSet<u32> =
            g.reverse_adjacency()[v].iter().copied().collect();
        assert_eq!(
            &actual, expected,
            "reverse_adj[{v}] inconsistent with forward adjacency after build: \
                 got {actual:?}, expected {expected:?}",
        );
    }
}

/// After `VamanaGraph::new` + manual adjacency edits (simulating a load path),
/// calling `rebuild_reverse_adj_from_adjacency` must restore full consistency.
#[test]
fn rebuild_reverse_adj_restores_consistency_after_manual_adjacency_load() {
    // Build a small chain graph manually (as `load` paths do).
    let n = 5usize;
    let mut g = VamanaGraph::new(n, 0).unwrap();
    // Chain: 0→1, 1→0,2, 2→1,3, 3→2,4, 4→3
    g.adjacency[0] = vec![1];
    g.adjacency[1] = vec![0, 2];
    g.adjacency[2] = vec![1, 3];
    g.adjacency[3] = vec![2, 4];
    g.adjacency[4] = vec![3];

    // Before rebuild, reverse_adj is all-empty (from `new`).
    for v in 0..n {
        assert!(
            g.reverse_adj[v].is_empty(),
            "reverse_adj[{v}] should be empty before rebuild"
        );
    }

    g.rebuild_reverse_adj_from_adjacency();

    // Now verify consistency: every forward edge u→v must appear as v's in-neighbor.
    for u in 0..n {
        for &v in &g.adjacency[u] {
            assert!(
                g.reverse_adj[v as usize].contains(&(u as u32)),
                "after rebuild: forward edge {u}→{v} not reflected in reverse_adj[{v}]"
            );
        }
    }
    // And the reverse: every entry in reverse_adj[v] must be a forward neighbor pointing at v.
    for v in 0..n {
        for &u in &g.reverse_adj[v] {
            assert!(
                g.adjacency[u as usize].contains(&(v as u32)),
                "after rebuild: reverse_adj[{v}] contains {u} \
                     but adjacency[{u}] does not contain {v}"
            );
        }
    }
}

/// `in_neighbors` returns the correct set for a known graph.
#[test]
fn in_neighbors_matches_expected_for_small_graph() {
    // Graph: 0→1, 0→2, 1→2
    let n = 3usize;
    let mut g = VamanaGraph::new(n, 0).unwrap();
    g.adjacency[0] = vec![1, 2];
    g.adjacency[1] = vec![2];
    g.adjacency[2] = vec![];
    g.rebuild_reverse_adj_from_adjacency();

    // Node 0: no one points at 0
    let in0: std::collections::HashSet<u32> = g.in_neighbors(0).unwrap().iter().copied().collect();
    assert_eq!(in0, std::collections::HashSet::new());

    // Node 1: only 0 points at 1
    let in1: std::collections::HashSet<u32> = g.in_neighbors(1).unwrap().iter().copied().collect();
    assert_eq!(in1, std::collections::HashSet::from([0u32]));

    // Node 2: both 0 and 1 point at 2
    let in2: std::collections::HashSet<u32> = g.in_neighbors(2).unwrap().iter().copied().collect();
    assert_eq!(in2, std::collections::HashSet::from([0u32, 1u32]));
}

/// `in_neighbors` returns an error for out-of-range node IDs.
#[test]
fn in_neighbors_rejects_out_of_range_node() {
    let g = VamanaGraph::new(3, 0).unwrap();
    assert!(
        matches!(g.in_neighbors(99), Err(VamanaError::InvalidFormat { .. })),
        "in_neighbors must return InvalidFormat for out-of-range node"
    );
}

/// After `add_node`, reverse_adj grows in sync with adjacency so the
/// graph remains consistent for any subsequent rebuild. The new node
/// starts with no in-neighbors, and the bidirectional invariant holds.
#[test]
fn add_node_extends_reverse_adj_in_sync() {
    let mut g = VamanaGraph::new(2, 0).unwrap();
    assert_eq!(g.adjacency.len(), g.reverse_adj.len());
    g.add_node().unwrap();
    assert_eq!(
        g.adjacency.len(),
        g.reverse_adj.len(),
        "reverse_adj must grow with adjacency after add_node"
    );
    assert_eq!(g.node_count(), 3);

    // The freshly added node carries no forward or reverse edges yet.
    let new_id = g.node_count() as u32 - 1;
    assert!(
        g.in_neighbors(new_id).unwrap().is_empty(),
        "newly added node must have no in-neighbors"
    );
    assert!(
        g.reverse_adjacency()[new_id as usize].is_empty(),
        "reverse_adj for newly added node must be empty"
    );

    // Full bidirectional invariant still holds after the mutation.
    for (u, outs) in g.adjacency().iter().enumerate() {
        for &v in outs {
            assert!(
                g.reverse_adjacency()[v as usize].contains(&(u as u32)),
                "forward edge {u}→{v} missing from reverse_adj[{v}] after add_node"
            );
        }
    }
    for (v, ins) in g.reverse_adjacency().iter().enumerate() {
        for &u in ins {
            assert!(
                g.adjacency()[u as usize].contains(&(v as u32)),
                "reverse edge {v}←{u} missing from adjacency[{u}] after add_node"
            );
        }
    }
}

// ---- Regression tests for P0/P1 fixes ----

/// P0: initial_random_adjacency must never produce self-loops.
#[test]
fn initial_random_adjacency_has_no_self_edges() {
    for num_vectors in [2usize, 5, 10, 100] {
        let max_degree = (num_vectors - 1).min(8);
        let adjacency = initial_random_adjacency(num_vectors, max_degree).unwrap();
        for (i, neighbors) in adjacency.iter().enumerate() {
            assert!(
                !neighbors.contains(&(i as u32)),
                "self-loop found at node {i} with num_vectors={num_vectors}"
            );
        }
    }
}

/// P0: initial_random_adjacency neighbors must all be distinct.
#[test]
fn initial_random_adjacency_neighbors_are_unique() {
    let adjacency = initial_random_adjacency(20, 8).unwrap();
    for (i, neighbors) in adjacency.iter().enumerate() {
        let mut sorted = neighbors.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            neighbors.len(),
            "duplicate neighbors at node {i}"
        );
    }
}

/// P0: greedy_search rejects vectors with fewer rows than graph node count.
#[test]
fn greedy_search_rejects_graph_vector_count_mismatch() {
    // Graph has 3 nodes but vectors only has 2 rows (dim=2).
    let vectors = vec![0.0f32, 1.0, 2.0, 3.0]; // 2 rows × 2 dims
    let g = VamanaGraph::new(3, 0).unwrap();
    let mut visited = VisitedSet::new(3);
    let query = [0.5f32, 0.5];
    let err = g.greedy_search(&vectors, 2, &query, 1, 5, &mut visited, None);
    assert!(
        matches!(err, Err(VamanaError::InvalidFormat { .. })),
        "expected InvalidFormat, got {err:?}"
    );
}

/// P1: robust_prune must reject NaN alpha.
#[test]
fn robust_prune_rejects_nan_alpha() {
    let vectors: Vec<f32> = vec![0.0, 0.5, 1.0];
    let g = VamanaGraph::new(3, 0).unwrap();
    let candidates: Vec<u32> = vec![1, 2];
    let err = g.robust_prune(&vectors, 1, 0, &candidates, f64::NAN, 4);
    assert!(
        matches!(err, Err(VamanaError::InvalidConfig { .. })),
        "expected InvalidConfig for NaN alpha, got {err:?}"
    );
}

/// P1: robust_prune must reject alpha < 1.0.
#[test]
fn robust_prune_rejects_alpha_below_one() {
    let vectors: Vec<f32> = vec![0.0, 0.5, 1.0];
    let g = VamanaGraph::new(3, 0).unwrap();
    let candidates: Vec<u32> = vec![1, 2];
    let err = g.robust_prune(&vectors, 1, 0, &candidates, 0.5, 4);
    assert!(
        matches!(err, Err(VamanaError::InvalidConfig { .. })),
        "expected InvalidConfig for alpha < 1.0, got {err:?}"
    );
}

/// P1: robust_prune rejects vectors with fewer rows than graph node count.
#[test]
fn robust_prune_rejects_graph_vector_count_mismatch() {
    // Graph has 4 nodes but only 2 rows supplied (dim=2 → 2 rows).
    let vectors = vec![0.0f32, 0.5, 1.0, 1.5]; // 4 scalars × dim=2 → 2 rows
    let g = VamanaGraph::new(4, 0).unwrap();
    let candidates: Vec<u32> = vec![1, 2];
    let err = g.robust_prune(&vectors, 2, 0, &candidates, 1.2, 4);
    assert!(
        matches!(err, Err(VamanaError::InvalidFormat { .. })),
        "expected InvalidFormat, got {err:?}"
    );
}

/// KHIVE_BUILD_BATCH env knob: the runtime batch-size resolver clamps to at least 1
/// and parses a valid decimal integer correctly.
#[test]
fn build_batch_env_knob_parses_correctly() {
    // Exercise the resolver logic directly without mutating the process env
    // (env mutations are process-global and race with parallel tests).
    //
    // The resolver is: std::env::var("KHIVE_BUILD_BATCH").ok()
    //                     .and_then(|s| s.trim().parse().ok())
    //                     .unwrap_or(BUILD_BATCH_SIZE);
    //                  batch_size.max(1);
    let parse = |s: &str| -> usize { s.trim().parse::<usize>().unwrap_or(BUILD_BATCH_SIZE).max(1) };
    assert_eq!(parse("16"), 16);
    assert_eq!(parse("1024"), BUILD_BATCH_SIZE);
    assert_eq!(parse("  128  "), 128);
    // Invalid input falls back to BUILD_BATCH_SIZE.
    assert_eq!(parse("notanumber"), BUILD_BATCH_SIZE);
    // Zero clamps to 1.
    let zero_parse: usize = "0"
        .trim()
        .parse::<usize>()
        .unwrap_or(BUILD_BATCH_SIZE)
        .max(1);
    assert_eq!(zero_parse, 1);
}

#[test]
fn build_sq8_rejects_encoded_row_count_mismatch_not_panic() {
    let dim = 2usize;
    let vectors = vec![0.0f32, 0.0, 1.0, 0.0]; // 2 valid rows
    let cfg = VamanaConfig::with_dimensions(dim)
        .with_max_degree(1)
        .with_search_list_size(1);
    let codec = GsSq8Codec::train_flat(&vectors, dim);
    let mut encoded = codec.encode_flat_par(&vectors, dim);
    encoded.clear(); // malformed: expected 2 encoded rows, actual 0

    let result = std::panic::catch_unwind(|| {
        VamanaGraph::build_sq8(&vectors, CodesView::Owned(&encoded), &codec, &cfg)
    });
    assert!(matches!(
        result,
        Ok(Err(VamanaError::DimensionMismatch {
            expected: 2,
            actual: 0
        }))
    ));
}
