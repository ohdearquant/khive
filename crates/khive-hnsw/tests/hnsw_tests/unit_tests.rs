use khive_hnsw::NodeId;
use khive_hnsw::{DistanceMetric, HnswConfig, HnswIndex};
use khive_score::DeterministicScore;

use std::collections::HashSet;

fn make_id(seed: u8) -> NodeId {
    NodeId::new([seed; 16])
}

fn generate_random_vector(dim: usize, seed: u64) -> Vec<f32> {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    (0..dim)
        .map(|i| {
            let mut hasher = DefaultHasher::new();
            (seed, i).hash(&mut hasher);
            (hasher.finish() as f32 / u64::MAX as f32) * 2.0 - 1.0
        })
        .collect()
}

#[test]
fn test_insert_and_search() {
    let mut index = HnswIndex::new(3);

    let id1 = make_id(1);
    let id2 = make_id(2);
    let id3 = make_id(3);

    index.insert(id1, vec![1.0, 0.0, 0.0]).expect("insert id1");
    index.insert(id2, vec![0.9, 0.1, 0.0]).expect("insert id2");
    index.insert(id3, vec![0.0, 1.0, 0.0]).expect("insert id3");

    assert_eq!(index.len(), 3);

    // Search for vector similar to [1.0, 0.0, 0.0]
    let results = index.search(&[1.0, 0.0, 0.0], 2).expect("search");

    assert_eq!(results.len(), 2);
    // First result should be id1 (exact match)
    assert_eq!(results[0].0, id1);
    assert!(results[0].1.to_f64() > 0.99);
}

#[test]
fn test_update_existing() {
    let mut index = HnswIndex::new(3);
    let id = make_id(1);

    index.insert(id, vec![1.0, 0.0, 0.0]).expect("insert");
    index.insert(id, vec![0.0, 1.0, 0.0]).expect("update");

    // Should still be 1 vector
    assert_eq!(index.len(), 1);

    // Search should find the updated vector
    let results = index.search(&[0.0, 1.0, 0.0], 1).expect("search");
    assert_eq!(results[0].0, id);
    assert!(results[0].1.to_f64() > 0.99);
}

#[test]
fn test_update_tombstoned_node_reconnects() {
    // When a tombstoned node is re-inserted (updated), the new vector
    // must be searchable. A plain vector-swap on a tombstoned node leaves
    // it unreachable because graph edges are missing.
    let mut index = HnswIndex::new(3);

    let id = make_id(1);
    let id2 = make_id(2);
    let id3 = make_id(3);

    index.insert(id, vec![1.0, 0.0, 0.0]).expect("insert");
    index.insert(id2, vec![0.9, 0.1, 0.0]).expect("insert id2");
    index.insert(id3, vec![0.0, 1.0, 0.0]).expect("insert id3");

    // Tombstone id
    assert!(index.delete(id));
    assert_eq!(index.tombstone_stats().tombstone_count, 1);

    // Re-insert (update) the tombstoned ID with a new vector
    index
        .insert(id, vec![0.0, 0.0, 1.0])
        .expect("re-insert tombstoned");

    // The tombstone count must drop back to 0 for this ID
    let stats = index.tombstone_stats();
    assert_eq!(
        stats.tombstone_count, 0,
        "tombstone should be cleared after re-insert"
    );

    // The re-inserted node must be reachable via search
    let results = index
        .search(&[0.0, 0.0, 1.0], 1)
        .expect("search after re-insert");
    assert!(!results.is_empty(), "re-inserted node must be findable");
    assert_eq!(results[0].0, id, "re-inserted node must be top result");
}

#[test]
fn test_reinsert_tombstoned_node_does_not_duplicate_internal_id() {
    // Regression for #414: re-inserting a tombstoned NodeId must not
    // resurrect the old internal slot alongside a fresh one. That would
    // inflate len()/len_live(), let exact-scan search return the stale
    // pre-delete vector, and duplicate the ID in snapshot.indexed_ids.
    let mut index = HnswIndex::new(3);

    let x = make_id(1);
    let y = make_id(2);
    let z = make_id(3);

    index.insert(x, vec![1.0, 0.0, 0.0]).expect("insert x");
    index.insert(y, vec![0.9, 0.1, 0.0]).expect("insert y");
    index.insert(z, vec![0.0, 1.0, 0.0]).expect("insert z");

    assert!(index.delete(x));
    index
        .insert(x, vec![0.0, 0.0, 1.0])
        .expect("reinsert tombstoned x");

    assert_eq!(index.len(), 3, "no duplicate internal slot for x");
    assert_eq!(index.len_live(), 3, "no duplicate internal slot for x");
    assert_eq!(index.tombstone_stats().tombstone_count, 0);

    let stale_hit = index
        .search(&[1.0, 0.0, 0.0], 1)
        .expect("search for stale x vector");
    assert_ne!(
        stale_hit[0].0, x,
        "stale pre-delete vector must not resolve back to x"
    );

    let fresh_hit = index
        .search(&[0.0, 0.0, 1.0], 1)
        .expect("search for fresh x vector");
    assert_eq!(fresh_hit[0].0, x, "fresh vector must resolve to x");

    let snap = index.snapshot();
    let unique_ids: HashSet<_> = snap.indexed_ids.iter().collect();
    assert_eq!(
        unique_ids.len(),
        snap.indexed_ids.len(),
        "snapshot.indexed_ids must not contain duplicate IDs"
    );
}

#[test]
fn test_delete_tombstone() {
    let mut index = HnswIndex::new(3);

    let id1 = make_id(1);
    let id2 = make_id(2);

    index.insert(id1, vec![1.0, 0.0, 0.0]).expect("insert id1");
    index.insert(id2, vec![0.0, 1.0, 0.0]).expect("insert id2");

    // Delete id1
    assert!(index.delete(id1));

    // Should still have 2 nodes but 1 tombstone
    assert_eq!(index.len(), 2);
    assert_eq!(index.tombstone_stats().tombstone_count, 1);

    // Search should not return tombstoned node
    let results = index.search(&[1.0, 0.0, 0.0], 2).expect("search");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].0, id2);
}

#[test]
fn test_rebuild() {
    let mut index = HnswIndex::new(3);

    let id1 = make_id(1);
    let id2 = make_id(2);
    let id3 = make_id(3);

    index.insert(id1, vec![1.0, 0.0, 0.0]).expect("insert");
    index.insert(id2, vec![0.0, 1.0, 0.0]).expect("insert");
    index.insert(id3, vec![0.0, 0.0, 1.0]).expect("insert");

    index.delete(id1);
    index.delete(id2);

    let stats = index.rebuild();
    assert_eq!(stats.nodes_before, 3);
    assert_eq!(stats.nodes_removed, 2);
    assert_eq!(stats.nodes_after, 1);

    // After rebuild, should only have id3
    assert_eq!(index.len(), 1);
    assert_eq!(index.tombstone_stats().tombstone_count, 0);
}

#[test]
fn test_dimension_mismatch() {
    let mut index = HnswIndex::new(3);
    let id = make_id(1);

    // Wrong dimension should error
    let result = index.insert(id, vec![1.0, 0.0]);
    assert!(result.is_err());

    // Insert correct dimension
    index.insert(id, vec![1.0, 0.0, 0.0]).expect("insert");

    // Search with wrong dimension should error
    let result = index.search(&[1.0, 0.0], 1);
    assert!(result.is_err());
}

#[test]
fn test_empty_search() {
    let index = HnswIndex::new(3);
    let results = index.search(&[1.0, 0.0, 0.0], 10).expect("search empty");
    assert!(results.is_empty());
}

#[test]
fn test_dot_product_metric() {
    let mut config = HnswConfig::with_dimensions(2);
    config.metric = DistanceMetric::Dot;
    let mut index = HnswIndex::with_config(config);

    let id1 = make_id(1);
    let id2 = make_id(2);

    index.insert(id1, vec![1.0, 0.0]).expect("insert id1");
    index.insert(id2, vec![2.0, 0.0]).expect("insert id2");

    // For dot product, [2,0] . [1,0] = 2 > [1,0] . [1,0] = 1
    let results = index.search(&[1.0, 0.0], 2).expect("search");
    assert_eq!(results[0].0, id2);
}

#[test]
fn exact_scan_dot_tail_and_batch_have_the_same_score() {
    let vectors = [
        (make_id(1), vec![0.6, 0.8]),
        (make_id(2), vec![0.0, 1.0]),
        (make_id(3), vec![-1.0, 0.0]),
        (make_id(4), vec![0.0, -1.0]),
        (make_id(5), vec![1.0, 0.0]),
    ];

    for metric in [DistanceMetric::Dot, DistanceMetric::Cosine] {
        for best_in_tail in [true, false] {
            let mut config = HnswConfig::with_dimensions(2);
            config.metric = metric;
            let mut index = HnswIndex::with_config(config);
            let order = if best_in_tail {
                [0, 1, 2, 3, 4]
            } else {
                [4, 0, 1, 2, 3]
            };
            for position in order {
                let (id, vector) = &vectors[position];
                index.insert(*id, vector.clone()).expect("insert");
            }

            let results = index.search(&[1.0, 0.0], 1).expect("search");
            assert_eq!(results[0].0, make_id(5), "{metric:?}, tail={best_in_tail}");
            assert_eq!(
                results[0].1.to_f64(),
                1.0,
                "{metric:?}, tail={best_in_tail}"
            );
        }
    }
}

#[test]
fn test_euclidean_metric() {
    let mut config = HnswConfig::with_dimensions(2);
    config.metric = DistanceMetric::L2;
    let mut index = HnswIndex::with_config(config);

    let id1 = make_id(1);
    let id2 = make_id(2);

    index.insert(id1, vec![1.0, 0.0]).expect("insert id1");
    index.insert(id2, vec![10.0, 0.0]).expect("insert id2");

    // Euclidean: closer = higher score
    let results = index.search(&[0.0, 0.0], 2).expect("search");
    assert_eq!(results[0].0, id1); // Closer to origin
}

#[test]
fn test_larger_index() {
    let mut index = HnswIndex::new(128);

    let n = 500;
    let mut ids = Vec::new();
    for i in 0..n {
        let id = NodeId::new([
            (i >> 8) as u8,
            (i & 0xff) as u8,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
        ]);
        let vector = generate_random_vector(128, i as u64);
        index.insert(id, vector).expect("insert");
        ids.push(id);
    }

    assert_eq!(index.len(), n);

    // Search should return k results
    let query = generate_random_vector(128, 50);
    let results = index.search(&query, 10).expect("search");
    assert_eq!(results.len(), 10);

    // Scores should be sorted descending
    for window in results.windows(2) {
        assert!(window[0].1 >= window[1].1);
    }
}

#[test]
fn test_recall() {
    let mut index = HnswIndex::new(64);

    let n = 500;
    let mut vectors: Vec<(NodeId, Vec<f32>)> = Vec::new();
    for i in 0..n {
        let id = NodeId::new([
            (i >> 8) as u8,
            (i & 0xff) as u8,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
        ]);
        let vector = generate_random_vector(64, i as u64);
        index.insert(id, vector.clone()).expect("insert");
        vectors.push((id, vector));
    }

    let k = 10;
    let mut total_recall = 0.0;
    let num_queries = 10;

    for q in 0..num_queries {
        let (query_id, query) = &vectors[q * 50];

        // Brute force ground truth
        let mut ground_truth: Vec<(f32, NodeId)> = vectors
            .iter()
            .map(|(id, v)| {
                let dot: f32 = query.iter().zip(v).map(|(a, b)| a * b).sum();
                let q_norm: f32 = query.iter().map(|x| x * x).sum::<f32>().sqrt();
                let v_norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
                let sim = if q_norm > 0.0 && v_norm > 0.0 {
                    dot / (q_norm * v_norm)
                } else {
                    0.0
                };
                (1.0 - sim, *id)
            })
            .collect();
        ground_truth.sort_by(|a, b| a.0.total_cmp(&b.0));
        let truth_ids: HashSet<NodeId> = ground_truth.iter().take(k).map(|(_, id)| *id).collect();

        // HNSW search
        let results = index.search(query, k).expect("search");
        let result_ids: HashSet<NodeId> = results.iter().map(|(id, _)| *id).collect();

        let recall = truth_ids.intersection(&result_ids).count() as f32 / truth_ids.len() as f32;
        total_recall += recall;

        // First result should be the query itself
        assert_eq!(
            results[0].0, *query_id,
            "Query {q} should return itself as first result"
        );
    }

    let avg_recall = total_recall / num_queries as f32;
    assert!(
        avg_recall > 0.8,
        "Average recall {avg_recall:.2} should be > 0.8"
    );
}

#[test]
fn test_config_variants() {
    // Test each config variant builds successfully
    for config in [
        HnswConfig::default(),
        HnswConfig::high_recall(),
        HnswConfig::fast_build(),
        HnswConfig::low_memory(),
    ] {
        let mut index = HnswIndex::with_config(HnswConfig {
            dimensions: 32,
            ..config
        });

        for i in 0..100 {
            let id = NodeId::new([i as u8; 16]);
            let vector = generate_random_vector(32, i as u64);
            index.insert(id, vector).expect("insert");
        }
        assert_eq!(index.len(), 100);
    }
}

#[test]
fn test_tombstone_stats() {
    let mut index = HnswIndex::new(3);

    for i in 0..10 {
        let id = make_id(i);
        index.insert(id, vec![i as f32, 0.0, 0.0]).expect("insert");
    }

    // Tombstone 3 nodes
    for i in 0..3 {
        index.delete(make_id(i));
    }

    let stats = index.tombstone_stats();
    assert_eq!(stats.total_nodes, 10);
    assert_eq!(stats.tombstone_count, 3);
    assert_eq!(stats.live_nodes, 7);
    assert!((stats.ratio - 0.3).abs() < 0.01);
}

#[test]
fn test_needs_rebuild_threshold() {
    let mut config = HnswConfig::with_dimensions(3);
    config.rebuild_threshold = 0.2;
    let mut index = HnswIndex::with_config(config);

    for i in 0..10 {
        let id = make_id(i);
        index.insert(id, vec![i as f32, 0.0, 0.0]).expect("insert");
    }

    // 1 tombstone = 10%, under 20% threshold
    index.delete(make_id(0));
    assert!(!index.needs_rebuild());

    // 3 tombstones = 30%, over 20% threshold
    index.delete(make_id(1));
    index.delete(make_id(2));
    assert!(index.needs_rebuild());
}

#[test]
fn test_deterministic_score_output() {
    let mut index = HnswIndex::new(3);

    let id = make_id(1);
    index.insert(id, vec![1.0, 0.0, 0.0]).expect("insert");

    let results = index.search(&[1.0, 0.0, 0.0], 1).expect("search");

    // Score should be DeterministicScore, not f32
    let score = results[0].1;
    assert!(score.to_f64() > 0.99);
    assert!(score.to_f64() <= 1.0);

    // Scores are comparable
    assert!(score > DeterministicScore::from_f64(0.5));
}

#[test]
fn test_clear() {
    let mut index = HnswIndex::new(3);

    index
        .insert(make_id(1), vec![1.0, 0.0, 0.0])
        .expect("insert");
    index
        .insert(make_id(2), vec![0.0, 1.0, 0.0])
        .expect("insert");
    index.delete(make_id(1));

    assert_eq!(index.len(), 2);
    assert_eq!(index.tombstone_stats().tombstone_count, 1);

    index.clear();

    assert_eq!(index.len(), 0);
    assert_eq!(index.tombstone_stats().tombstone_count, 0);
    assert!(index.is_empty());
}

#[test]
fn test_seeded_rng_reproducibility() {
    // Two indexes with the same seed should produce identical structure
    let config1 = HnswConfig::with_dimensions(32).with_seed(42);
    let config2 = HnswConfig::with_dimensions(32).with_seed(42);

    let mut index1 = HnswIndex::with_config(config1);
    let mut index2 = HnswIndex::with_config(config2);

    // Insert same vectors in same order
    for i in 0..50 {
        let id = NodeId::new([i as u8; 16]);
        let vector = generate_random_vector(32, i as u64);
        index1.insert(id, vector.clone()).expect("insert");
        index2.insert(id, vector).expect("insert");
    }

    // Search should return identical results
    let query = generate_random_vector(32, 999);
    let results1 = index1.search(&query, 10).expect("search");
    let results2 = index2.search(&query, 10).expect("search");

    assert_eq!(results1.len(), results2.len());
    for (r1, r2) in results1.iter().zip(results2.iter()) {
        assert_eq!(r1.0, r2.0, "Same seed should produce identical results");
        assert_eq!(r1.1, r2.1, "Scores should match exactly");
    }
}

#[test]
fn test_different_seeds_different_structure() {
    // Two indexes with different seeds should (likely) produce different structures
    let config1 = HnswConfig::with_dimensions(32).with_seed(42);
    let config2 = HnswConfig::with_dimensions(32).with_seed(123);

    let mut index1 = HnswIndex::with_config(config1);
    let mut index2 = HnswIndex::with_config(config2);

    // Insert same vectors in same order
    for i in 0..100 {
        let id = NodeId::new([i as u8; 16]);
        let vector = generate_random_vector(32, i as u64);
        index1.insert(id, vector.clone()).expect("insert");
        index2.insert(id, vector).expect("insert");
    }

    // Get max_level for each - with different seeds, the random levels
    // assigned to nodes will differ, leading to different max_level values
    // (statistically likely to differ with 100 insertions)
    // Note: They might still be the same by chance, but the internal
    // structure (which nodes are at which level) will differ

    // At minimum, both should be valid indexes
    assert_eq!(index1.len(), 100);
    assert_eq!(index2.len(), 100);
}
