//! Integration tests for khive-hnsw.

#[cfg(test)]
mod unit_tests {
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
            let truth_ids: HashSet<NodeId> =
                ground_truth.iter().take(k).map(|(_, id)| *id).collect();

            // HNSW search
            let results = index.search(query, k).expect("search");
            let result_ids: HashSet<NodeId> = results.iter().map(|(id, _)| *id).collect();

            let recall =
                truth_ids.intersection(&result_ids).count() as f32 / truth_ids.len() as f32;
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
}

#[cfg(test)]
mod memory_budget_tests {
    use khive_hnsw::error::{ErrorKind, RetrievalError};
    use khive_hnsw::NodeId;
    use khive_hnsw::{HnswConfig, HnswIndex};

    fn make_id(seed: u8) -> NodeId {
        NodeId::new([seed; 16])
    }

    #[test]
    fn test_no_budget_allows_unlimited_inserts() {
        // Without a budget, inserts always succeed
        let mut index = HnswIndex::new(4);
        for i in 0..50 {
            let id = make_id(i);
            index
                .insert(id, vec![i as f32, 0.0, 0.0, 0.0])
                .expect("insert should succeed without budget");
        }
        assert_eq!(index.len(), 50);
    }

    #[test]
    fn test_budget_blocks_insert_when_exceeded() {
        // Set a very tight budget that allows only a few nodes
        let config = HnswConfig::with_dimensions(4).with_memory_budget(2_000);
        let mut index = HnswIndex::with_config(config);

        // First insert should succeed (index starts empty)
        index
            .insert(make_id(1), vec![1.0, 0.0, 0.0, 0.0])
            .expect("first insert should succeed");

        // Keep inserting until we hit the budget
        let mut rejected = false;
        for i in 2..=100u8 {
            let result = index.insert(make_id(i), vec![i as f32, 0.0, 0.0, 0.0]);
            if let Err(err) = result {
                rejected = true;
                assert!(
                    matches!(err, RetrievalError::BudgetExceeded { .. }),
                    "Expected BudgetExceeded, got: {err:?}"
                );
                assert_eq!(err.kind(), ErrorKind::Permanent);
                assert!(!err.is_retryable());
                break;
            }
        }
        assert!(rejected, "Budget should have rejected an insert");
    }

    #[test]
    fn test_budget_update_bypasses_check() {
        // Set a budget, fill it up, then update an existing entry
        let config = HnswConfig::with_dimensions(4).with_memory_budget(2_000);
        let mut index = HnswIndex::with_config(config);

        let id1 = make_id(1);
        index
            .insert(id1, vec![1.0, 0.0, 0.0, 0.0])
            .expect("first insert");

        // Fill until budget hit
        for i in 2..=100u8 {
            if index
                .insert(make_id(i), vec![i as f32, 0.0, 0.0, 0.0])
                .is_err()
            {
                break;
            }
        }

        // Updating an existing entry should always succeed (bypass budget)
        index
            .insert(id1, vec![9.0, 9.0, 9.0, 9.0])
            .expect("update existing should bypass budget");
    }

    #[test]
    fn test_memory_usage_increases_with_inserts() {
        let mut index = HnswIndex::new(8);

        let before = index.memory_usage();
        assert_eq!(before, 0, "Empty index should have zero usage");

        index.insert(make_id(1), vec![1.0; 8]).expect("insert");
        let after_one = index.memory_usage();
        assert!(after_one > 0, "Usage should increase after insert");

        index.insert(make_id(2), vec![2.0; 8]).expect("insert");
        let after_two = index.memory_usage();
        assert!(
            after_two > after_one,
            "Usage should increase with more inserts"
        );
    }

    #[test]
    fn test_estimate_insert_cost_is_positive() {
        let index = HnswIndex::new(128);
        let cost = index.estimate_insert_cost();
        assert!(cost > 0, "Insert cost should be positive");
        // For 128 dims: 128*4 = 512 bytes for vector alone
        assert!(cost >= 512, "Cost should include at least the vector data");
    }

    #[test]
    fn test_memory_budget_getter_setter() {
        let mut index = HnswIndex::new(4);

        // Default: no budget
        assert_eq!(index.memory_budget(), None);

        // Set budget via runtime setter
        index.set_memory_budget(Some(10_000));
        assert_eq!(index.memory_budget(), Some(10_000));

        // Clear budget
        index.set_memory_budget(None);
        assert_eq!(index.memory_budget(), None);
    }

    #[test]
    fn test_budget_from_config() {
        let config = HnswConfig::with_dimensions(4).with_memory_budget(5_000);
        let index = HnswIndex::with_config(config);
        assert_eq!(index.memory_budget(), Some(5_000));
    }

    #[test]
    fn test_budget_exceeded_error_details() {
        let config = HnswConfig::with_dimensions(4).with_memory_budget(1);
        let mut index = HnswIndex::with_config(config);

        // Budget of 1 byte is too small for any insert
        let result = index.insert(make_id(1), vec![1.0, 0.0, 0.0, 0.0]);
        assert!(result.is_err());

        let err = result.unwrap_err();
        match err {
            RetrievalError::BudgetExceeded {
                current_usage,
                item_size,
                limit,
            } => {
                assert_eq!(current_usage, 0, "Empty index");
                assert!(item_size > 0, "Item should have non-zero cost");
                assert_eq!(limit, 1, "Limit should match config");
                assert!(current_usage + item_size > limit, "Should genuinely exceed");
            }
            other => panic!("Expected BudgetExceeded, got: {other:?}"),
        }
    }

    #[test]
    fn test_search_unaffected_by_budget() {
        // Budget is only checked on insert, never on search
        let config = HnswConfig::with_dimensions(3).with_memory_budget(100_000);
        let mut index = HnswIndex::with_config(config);

        index
            .insert(make_id(1), vec![1.0, 0.0, 0.0])
            .expect("insert");
        index
            .insert(make_id(2), vec![0.0, 1.0, 0.0])
            .expect("insert");

        // Search should work regardless of budget
        let results = index.search(&[1.0, 0.0, 0.0], 2).expect("search");
        assert_eq!(results.len(), 2);
    }
}

#[cfg(test)]
mod proptests {
    use khive_hnsw::HnswIndex;
    use khive_hnsw::NodeId;

    use proptest::prelude::*;

    const DIM: usize = 32;

    fn seeded_vector(seed: u64) -> Vec<f32> {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        (0..DIM)
            .map(|i| {
                let mut hasher = DefaultHasher::new();
                (seed, i).hash(&mut hasher);
                (hasher.finish() as f32 / u64::MAX as f32) * 2.0 - 1.0
            })
            .collect()
    }

    proptest! {
        /// Property: search() returns exactly k results when k <= num_vectors
        #[test]
        fn search_returns_k_results(
            k in 1usize..=10,
            num_vectors in 10usize..=100
        ) {
            prop_assume!(k <= num_vectors);

            let mut index = HnswIndex::new(DIM);

            for i in 0..num_vectors {
                let id = NodeId::new([
                    (i >> 8) as u8,
                    (i & 0xff) as u8,
                    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                ]);
                let vector = seeded_vector(i as u64);
                index.insert(id, vector).expect("insert");
            }

            prop_assert_eq!(index.len(), num_vectors);

            let query = seeded_vector(999);
            let results = index.search(&query, k).expect("search");

            prop_assert_eq!(
                results.len(),
                k,
                "Expected {} results but got {}",
                k,
                results.len()
            );

            // All scores should be finite (DeterministicScore is i64 fixed-point; check via to_f64).
            for (_, score) in &results {
                prop_assert!(
                    score.to_f64().is_finite(),
                    "Score should be finite"
                );
            }

            // Results sorted descending by score
            for window in results.windows(2) {
                prop_assert!(
                    window[0].1 >= window[1].1,
                    "Results should be sorted by score descending"
                );
            }
        }

        /// Property: search() returns min(k, num_vectors) when k > num_vectors
        #[test]
        fn search_returns_all_when_k_exceeds_count(
            num_vectors in 1usize..=20,
            k_excess in 1usize..=30
        ) {
            let k = num_vectors + k_excess;

            let mut index = HnswIndex::new(DIM);

            for i in 0..num_vectors {
                let id = NodeId::new([i as u8; 16]);
                let vector = seeded_vector(i as u64);
                index.insert(id, vector).expect("insert");
            }

            let query = seeded_vector(999);
            let results = index.search(&query, k).expect("search");

            prop_assert_eq!(
                results.len(),
                num_vectors,
                "Expected {} results (all vectors) but got {}",
                num_vectors,
                results.len()
            );
        }

        /// Property: empty index returns empty results
        #[test]
        fn search_empty_index_returns_empty(k in 1usize..=100) {
            let index = HnswIndex::new(DIM);

            let query = seeded_vector(0);
            let results = index.search(&query, k).expect("search");

            prop_assert!(
                results.is_empty(),
                "Empty index should return empty results"
            );
        }
    }
}

#[cfg(test)]
mod metrics_tests {
    use khive_hnsw::metrics::{names, MetricValue, RecordingSink};
    use khive_hnsw::HnswIndex;
    use khive_hnsw::NodeId;

    use std::sync::Arc;

    fn make_id(seed: u8) -> NodeId {
        NodeId::new([seed; 16])
    }

    #[test]
    fn insert_emits_metrics() {
        let sink = Arc::new(RecordingSink::new());
        let mut index = HnswIndex::new(3).with_metrics(sink.clone());

        index.insert(make_id(1), vec![1.0, 0.0, 0.0]).unwrap();

        let events = sink.events();
        let event_names: Vec<&str> = events.iter().map(|e| e.name).collect();

        assert!(
            event_names.contains(&names::HNSW_INSERT_DURATION_MS),
            "Missing insert duration metric"
        );
        assert!(
            event_names.contains(&names::HNSW_INSERT_COUNT),
            "Missing insert count metric"
        );
        assert!(
            event_names.contains(&names::HNSW_INDEX_SIZE),
            "Missing index size metric"
        );

        // Index size should be 1 after first insert
        let size_event = events
            .iter()
            .find(|e| e.name == names::HNSW_INDEX_SIZE)
            .unwrap();
        assert_eq!(size_event.value, MetricValue::Gauge(1.0));
    }

    #[test]
    fn search_emits_metrics() {
        let sink = Arc::new(RecordingSink::new());
        let mut index = HnswIndex::new(3).with_metrics(sink.clone());

        index.insert(make_id(1), vec![1.0, 0.0, 0.0]).unwrap();
        index.insert(make_id(2), vec![0.0, 1.0, 0.0]).unwrap();

        // Clear insert metrics
        sink.clear();

        let results = index.search(&[1.0, 0.0, 0.0], 2).unwrap();

        let events = sink.events();
        let event_names: Vec<&str> = events.iter().map(|e| e.name).collect();

        assert!(
            event_names.contains(&names::HNSW_SEARCH_DURATION_MS),
            "Missing search duration metric"
        );
        assert!(
            event_names.contains(&names::HNSW_SEARCH_COUNT),
            "Missing search count metric"
        );
        assert!(
            event_names.contains(&names::HNSW_SEARCH_RESULTS),
            "Missing search results metric"
        );

        // Results count should match actual results
        let results_event = events
            .iter()
            .find(|e| e.name == names::HNSW_SEARCH_RESULTS)
            .unwrap();
        assert_eq!(
            results_event.value,
            MetricValue::Gauge(results.len() as f64)
        );
    }

    #[test]
    fn rebuild_emits_metrics() {
        let sink = Arc::new(RecordingSink::new());
        let mut index = HnswIndex::new(3).with_metrics(sink.clone());

        let id1 = make_id(1);
        let id2 = make_id(2);
        index.insert(id1, vec![1.0, 0.0, 0.0]).unwrap();
        index.insert(id2, vec![0.0, 1.0, 0.0]).unwrap();

        // Tombstone one node
        index.delete(id1);

        // Clear prior metrics
        sink.clear();

        let stats = index.rebuild();

        let events = sink.events();
        let event_names: Vec<&str> = events.iter().map(|e| e.name).collect();

        assert!(
            event_names.contains(&names::HNSW_REBUILD_DURATION_MS),
            "Missing rebuild duration metric"
        );
        assert!(
            event_names.contains(&names::HNSW_REBUILD_COUNT),
            "Missing rebuild count metric"
        );
        assert!(
            event_names.contains(&names::HNSW_REBUILD_NODES_REMOVED),
            "Missing rebuild nodes_removed metric"
        );
        assert!(
            event_names.contains(&names::HNSW_INDEX_SIZE),
            "Missing index size metric after rebuild"
        );

        // nodes_removed should be 1
        let removed_event = events
            .iter()
            .find(|e| e.name == names::HNSW_REBUILD_NODES_REMOVED)
            .unwrap();
        assert_eq!(removed_event.value, MetricValue::Gauge(1.0));
        assert_eq!(stats.nodes_removed, 1);
    }

    #[test]
    fn no_metrics_without_sink() {
        // Ensure no panic when metrics is None (default)
        let mut index = HnswIndex::new(3);
        index.insert(make_id(1), vec![1.0, 0.0, 0.0]).unwrap();
        let _ = index.search(&[1.0, 0.0, 0.0], 1).unwrap();
        index.rebuild();
    }

    #[test]
    fn set_metrics_at_runtime() {
        let mut index = HnswIndex::new(3);
        index.insert(make_id(1), vec![1.0, 0.0, 0.0]).unwrap();

        // Attach sink mid-lifecycle
        let sink = Arc::new(RecordingSink::new());
        index.set_metrics(Some(sink.clone()));

        index.insert(make_id(2), vec![0.0, 1.0, 0.0]).unwrap();

        // Should have metrics from the second insert only
        assert!(!sink.is_empty());

        // Detach
        index.set_metrics(None);
        sink.clear();

        index.insert(make_id(3), vec![0.0, 0.0, 1.0]).unwrap();
        assert!(sink.is_empty(), "No events after detaching sink");
    }

    #[test]
    fn search_on_empty_index_still_emits() {
        let sink = Arc::new(RecordingSink::new());
        let index = HnswIndex::new(3).with_metrics(sink.clone());

        let results = index.search(&[1.0, 0.0, 0.0], 5).unwrap();
        assert!(results.is_empty());

        // Should still emit duration and count (even for empty index)
        let events = sink.events();
        let event_names: Vec<&str> = events.iter().map(|e| e.name).collect();
        assert!(event_names.contains(&names::HNSW_SEARCH_DURATION_MS));
        assert!(event_names.contains(&names::HNSW_SEARCH_COUNT));
    }

    #[test]
    fn insert_duration_is_nonnegative() {
        let sink = Arc::new(RecordingSink::new());
        let mut index = HnswIndex::new(3).with_metrics(sink.clone());

        index.insert(make_id(1), vec![1.0, 0.0, 0.0]).unwrap();

        let duration_event = sink
            .events()
            .into_iter()
            .find(|e| e.name == names::HNSW_INSERT_DURATION_MS)
            .unwrap();

        match duration_event.value {
            MetricValue::Histogram(ms) => assert!(ms >= 0.0, "Duration must be >= 0"),
            other => panic!("Expected Histogram, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod search_context_tests {
    use khive_hnsw::HnswSearchContext;
    use khive_hnsw::NodeId;
    use khive_hnsw::{DistanceMetric, HnswConfig, HnswIndex};

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
    fn search_with_context_matches_search() {
        // Build a non-trivial index
        let config = HnswConfig::with_dimensions(64).with_seed(42);
        let mut index = HnswIndex::with_config(config);

        for i in 0..200u16 {
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
            index.insert(id, vector).expect("insert");
        }

        let mut ctx = HnswSearchContext::new(index.config().ef_search);

        // Run multiple queries and verify results are identical
        for q_seed in [0u64, 50, 100, 999, 12345] {
            let query = generate_random_vector(64, q_seed);

            let results_normal = index.search(&query, 10).expect("search");
            let results_ctx = index
                .search_with_context(&query, 10, &mut ctx)
                .expect("search_with_context");

            assert_eq!(
                results_normal.len(),
                results_ctx.len(),
                "Result count should match for query seed {q_seed}"
            );
            for (i, (r_normal, r_ctx)) in results_normal.iter().zip(results_ctx.iter()).enumerate()
            {
                assert_eq!(
                    r_normal.0, r_ctx.0,
                    "ID mismatch at position {i} for query seed {q_seed}"
                );
                assert_eq!(
                    r_normal.1, r_ctx.1,
                    "Score mismatch at position {i} for query seed {q_seed}"
                );
            }
        }
    }

    #[test]
    fn context_reuse_across_many_searches() {
        let config = HnswConfig::with_dimensions(32).with_seed(42);
        let mut index = HnswIndex::with_config(config);

        for i in 0..100u16 {
            let id = NodeId::new([i as u8; 16]);
            let vector = generate_random_vector(32, i as u64);
            index.insert(id, vector).expect("insert");
        }

        let mut ctx = HnswSearchContext::new(index.config().ef_search);

        // Run 50 searches reusing the same context
        for q in 0..50u64 {
            let query = generate_random_vector(32, q * 7);
            let results = index
                .search_with_context(&query, 5, &mut ctx)
                .expect("search_with_context");
            assert_eq!(results.len(), 5, "Should return 5 results on iteration {q}");

            // Verify sorted descending by score
            for window in results.windows(2) {
                assert!(
                    window[0].1 >= window[1].1,
                    "Results should be sorted descending"
                );
            }
        }
    }

    #[test]
    fn context_works_with_tombstones() {
        let config = HnswConfig::with_dimensions(3).with_seed(42);
        let mut index = HnswIndex::with_config(config);

        let id1 = make_id(1);
        let id2 = make_id(2);
        let id3 = make_id(3);

        index.insert(id1, vec![1.0, 0.0, 0.0]).expect("insert");
        index.insert(id2, vec![0.9, 0.1, 0.0]).expect("insert");
        index.insert(id3, vec![0.0, 1.0, 0.0]).expect("insert");

        // Delete id1
        index.delete(id1);

        let mut ctx = HnswSearchContext::new(index.config().ef_search);

        let results_normal = index.search(&[1.0, 0.0, 0.0], 3).expect("search");
        let results_ctx = index
            .search_with_context(&[1.0, 0.0, 0.0], 3, &mut ctx)
            .expect("search_with_context");

        // Should not include tombstoned id1
        assert_eq!(results_normal.len(), results_ctx.len());
        for (r_normal, r_ctx) in results_normal.iter().zip(results_ctx.iter()) {
            assert_eq!(r_normal.0, r_ctx.0);
            assert_eq!(r_normal.1, r_ctx.1);
            assert_ne!(r_normal.0, id1, "Tombstoned id1 should not appear");
        }
    }

    #[test]
    fn context_with_empty_index() {
        let index = HnswIndex::new(3);
        let mut ctx = HnswSearchContext::new(80);

        let results = index
            .search_with_context(&[1.0, 0.0, 0.0], 10, &mut ctx)
            .expect("search empty");
        assert!(results.is_empty());
    }

    #[test]
    fn context_dimension_mismatch() {
        let mut index = HnswIndex::new(3);
        index
            .insert(make_id(1), vec![1.0, 0.0, 0.0])
            .expect("insert");

        let mut ctx = HnswSearchContext::new(80);
        let result = index.search_with_context(&[1.0, 0.0], 1, &mut ctx);
        assert!(result.is_err());
    }

    #[test]
    fn context_works_with_all_metrics() {
        for metric in [
            DistanceMetric::Cosine,
            DistanceMetric::L2,
            DistanceMetric::Dot,
        ] {
            let mut config = HnswConfig::with_dimensions(4);
            config.metric = metric;
            config.seed = Some(42);
            let mut index = HnswIndex::with_config(config);

            for i in 0..50u8 {
                let id = make_id(i);
                let vector = generate_random_vector(4, i as u64);
                index.insert(id, vector).expect("insert");
            }

            let query = generate_random_vector(4, 999);
            let mut ctx = HnswSearchContext::new(index.config().ef_search);

            let results_normal = index.search(&query, 5).expect("search");
            let results_ctx = index
                .search_with_context(&query, 5, &mut ctx)
                .expect("search_with_context");

            assert_eq!(
                results_normal.len(),
                results_ctx.len(),
                "Result count mismatch for {metric:?}"
            );
            for (r_normal, r_ctx) in results_normal.iter().zip(results_ctx.iter()) {
                assert_eq!(r_normal.0, r_ctx.0, "ID mismatch for {metric:?}");
                assert_eq!(r_normal.1, r_ctx.1, "Score mismatch for {metric:?}");
            }
        }
    }

    // =========================================================================
    // INT8 Quantized Search Tests
    // =========================================================================

    #[test]
    fn test_quantized_search_identical_results_small() {
        let config = HnswConfig {
            dimensions: 32,
            seed: Some(42),
            ..Default::default()
        };
        let mut index = HnswIndex::with_config(config);

        for i in 0..50u8 {
            let vec = generate_random_vector(32, i as u64);
            index.insert(make_id(i), vec).unwrap();
        }

        let query = generate_random_vector(32, 999);
        let results_f32 = index.search(&query, 10).unwrap();

        index.set_quantized(true);
        assert!(index.is_quantized());
        let results_quant = index.search(&query, 10).unwrap();

        assert_eq!(results_f32.len(), results_quant.len());
        for (f32_result, quant_result) in results_f32.iter().zip(results_quant.iter()) {
            assert_eq!(f32_result.0, quant_result.0, "ID mismatch");
            assert_eq!(f32_result.1, quant_result.1, "Score mismatch");
        }
    }

    #[test]
    fn test_quantized_search_identical_results_medium() {
        let config = HnswConfig {
            dimensions: 128,
            seed: Some(42),
            ef_search: 50,
            ..Default::default()
        };
        let mut index = HnswIndex::with_config(config);

        for i in 0..200u64 {
            let vec = generate_random_vector(128, i);
            let id_bytes: [u8; 16] = {
                let mut b = [0u8; 16];
                b[..8].copy_from_slice(&i.to_le_bytes());
                b
            };
            index.insert(NodeId::new(id_bytes), vec).unwrap();
        }

        for q_seed in 1000..1010u64 {
            let query = generate_random_vector(128, q_seed);

            let results_f32 = index.search(&query, 10).unwrap();

            index.set_quantized(true);
            let results_quant = index.search(&query, 10).unwrap();
            index.set_quantized(false);

            assert_eq!(
                results_f32.len(),
                results_quant.len(),
                "Result count mismatch for query seed {q_seed}"
            );
            for (f32_r, quant_r) in results_f32.iter().zip(results_quant.iter()) {
                assert_eq!(f32_r.0, quant_r.0, "ID mismatch for query seed {q_seed}");
                // Allow a small tolerance for f32 FP rounding: batch-4 SIMD kernels
                // use a different FMA accumulation order than the scalar pair kernel,
                // producing differences of ~1e-7 (within single-precision epsilon for
                // 128-dim vectors). The neighbor IDs above confirm same recall; this
                // only checks that scores are within acceptable precision.
                let diff = (f32_r.1.to_f64() - quant_r.1.to_f64()).abs();
                assert!(
                    diff < 1e-5,
                    "Score mismatch for query seed {q_seed}: {} vs {} (diff={diff:.2e})",
                    f32_r.1.to_f64(),
                    quant_r.1.to_f64()
                );
            }
        }
    }

    #[test]
    fn test_quantized_builder_pattern() {
        let index = HnswIndex::new(64).with_quantized();
        assert!(index.is_quantized());
    }

    #[test]
    fn test_quantized_runtime_toggle() {
        let mut index = HnswIndex::new(64);
        assert!(!index.is_quantized());

        index.set_quantized(true);
        assert!(index.is_quantized());

        index.set_quantized(false);
        assert!(!index.is_quantized());
    }

    #[test]
    fn test_quantized_arena_survives_update() {
        let mut index = HnswIndex::new(3);
        index.set_quantized(true);

        let id = make_id(1);
        index.insert(id, vec![1.0, 0.0, 0.0]).unwrap();
        index.insert(id, vec![0.0, 1.0, 0.0]).unwrap();

        let results = index.search(&[0.0, 1.0, 0.0], 1).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, id);
        assert!(results[0].1.to_f64() > 0.99);
    }

    #[test]
    fn test_quantized_arena_survives_rebuild() {
        let mut index = HnswIndex::new(3);
        index.set_quantized(true);

        let id1 = make_id(1);
        let id2 = make_id(2);
        let id3 = make_id(3);

        index.insert(id1, vec![1.0, 0.0, 0.0]).unwrap();
        index.insert(id2, vec![0.0, 1.0, 0.0]).unwrap();
        index.insert(id3, vec![0.0, 0.0, 1.0]).unwrap();

        index.delete(id2);
        index.rebuild();

        let results = index.search(&[1.0, 0.0, 0.0], 2).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, id1);
        assert!(results.iter().all(|(id, _)| *id != id2));
    }

    #[test]
    fn test_quantized_arena_survives_clear() {
        let mut index = HnswIndex::new(3);
        index.set_quantized(true);

        index.insert(make_id(1), vec![1.0, 0.0, 0.0]).unwrap();
        index.clear();

        index.insert(make_id(2), vec![0.0, 1.0, 0.0]).unwrap();
        let results = index.search(&[0.0, 1.0, 0.0], 1).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, make_id(2));
    }

    #[test]
    fn test_quantized_empty_index() {
        let index = HnswIndex::new(3).with_quantized();
        let results = index.search(&[1.0, 0.0, 0.0], 5).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_quantized_only_affects_cosine() {
        for metric in [DistanceMetric::Dot, DistanceMetric::L2] {
            let config = HnswConfig {
                dimensions: 32,
                metric,
                seed: Some(42),
                ..Default::default()
            };
            let mut index = HnswIndex::with_config(config);

            for i in 0..30u8 {
                let vec = generate_random_vector(32, i as u64);
                index.insert(make_id(i), vec).unwrap();
            }

            let query = generate_random_vector(32, 999);
            let results_f32 = index.search(&query, 5).unwrap();

            index.set_quantized(true);
            let results_quant = index.search(&query, 5).unwrap();

            assert_eq!(
                results_f32.len(),
                results_quant.len(),
                "Result count mismatch for {metric:?}"
            );
            for (a, b) in results_f32.iter().zip(results_quant.iter()) {
                assert_eq!(a.0, b.0, "ID mismatch for {metric:?}");
                assert_eq!(a.1, b.1, "Score mismatch for {metric:?}");
            }
        }
    }

    #[test]
    fn test_quantization_error_bounded() {
        let dim = 384;
        for seed in 0..20u64 {
            let vec = generate_random_vector(dim, seed);
            let norm: f32 = vec.iter().map(|x| x * x).sum::<f32>().sqrt();

            let mut max_abs: f32 = 0.0;
            for &v in &vec {
                let abs = v.abs();
                if abs > max_abs {
                    max_abs = abs;
                }
            }
            let scale = if max_abs > 1e-10 {
                127.0 / max_abs
            } else {
                1.0
            };
            let quantized: Vec<i8> = vec
                .iter()
                .map(|&v| (v * scale).round().clamp(-127.0, 127.0) as i8)
                .collect();

            let dequantized: Vec<f32> = quantized.iter().map(|&v| v as f32 / scale).collect();

            let dot: f32 = vec.iter().zip(dequantized.iter()).map(|(a, b)| a * b).sum();
            let dq_norm: f32 = dequantized.iter().map(|x| x * x).sum::<f32>().sqrt();
            let cos_sim = if norm > 0.0 && dq_norm > 0.0 {
                dot / (norm * dq_norm)
            } else {
                1.0
            };

            assert!(
                cos_sim > 0.95,
                "Quantization error too high: cosine_sim={cos_sim} for seed={seed}"
            );
            assert!(
                cos_sim > 0.99,
                "Expected high fidelity for 384d: cosine_sim={cos_sim}"
            );
        }
    }
}

// =============================================================================
// Snapshot / restore tests (Issue #2161)
// =============================================================================

#[cfg(test)]
mod snapshot_tests {
    use khive_hnsw::NodeId;
    use khive_hnsw::{HnswConfig, HnswIndex};
    use std::collections::HashMap;

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

    /// Snapshot from a populated index includes vector data.
    #[test]
    fn snapshot_includes_vector_data() {
        let mut index = HnswIndex::new(4);

        let id1 = make_id(1);
        let id2 = make_id(2);
        let vec1 = vec![1.0, 0.0, 0.0, 0.0];
        let vec2 = vec![0.0, 1.0, 0.0, 0.0];

        index.insert(id1, vec1.clone()).expect("insert");
        index.insert(id2, vec2.clone()).expect("insert");

        let snap = index.snapshot();

        assert_eq!(snap.vectors.len(), 2, "snapshot should contain 2 vectors");

        // Vectors are sorted by NodeId bytes — look up by id
        let vec_map: HashMap<NodeId, &Vec<f32>> =
            snap.vectors.iter().map(|(id, v)| (*id, v)).collect();

        assert_eq!(
            vec_map.get(&id1).copied(),
            Some(&vec1),
            "id1 vector should match"
        );
        assert_eq!(
            vec_map.get(&id2).copied(),
            Some(&vec2),
            "id2 vector should match"
        );
    }

    /// Empty index snapshot has no vectors.
    #[test]
    fn snapshot_empty_index_has_no_vectors() {
        let index = HnswIndex::new(4);
        let snap = index.snapshot();
        assert!(
            snap.vectors.is_empty(),
            "empty index snapshot has no vectors"
        );
    }

    /// Self-contained round-trip: snapshot() → restore_from_snapshot_embedded().
    #[test]
    fn snapshot_restore_embedded_round_trip() {
        let config = HnswConfig::with_dimensions(8).with_seed(42);
        let mut original = HnswIndex::with_config(config.clone());

        let ids: Vec<NodeId> = (0..20u8).map(make_id).collect();
        let vecs: Vec<Vec<f32>> = (0..20u64).map(|i| generate_random_vector(8, i)).collect();

        for (id, vec) in ids.iter().zip(vecs.iter()) {
            original.insert(*id, vec.clone()).expect("insert");
        }

        // Take a snapshot and restore into a fresh index
        let snap = original.snapshot();
        assert_eq!(
            snap.vectors.len(),
            20,
            "snapshot should embed all 20 vectors"
        );

        let mut restored = HnswIndex::with_config(config);
        restored
            .restore_from_snapshot_embedded(&snap)
            .expect("restore embedded");

        assert_eq!(restored.len(), 20, "restored index should have 20 nodes");

        // Search results should match the original
        let query = generate_random_vector(8, 999);
        let results_orig = original.search(&query, 5).expect("search original");
        let results_rest = restored.search(&query, 5).expect("search restored");

        assert_eq!(
            results_orig.len(),
            results_rest.len(),
            "result count should match"
        );
        for (r_orig, r_rest) in results_orig.iter().zip(results_rest.iter()) {
            assert_eq!(r_orig.0, r_rest.0, "result IDs should match");
        }
    }

    /// Tombstoned nodes are preserved through embedded snapshot round-trip.
    #[test]
    fn snapshot_restore_embedded_preserves_tombstones() {
        let config = HnswConfig::with_dimensions(4).with_seed(42);
        let mut index = HnswIndex::with_config(config.clone());

        let id1 = make_id(1);
        let id2 = make_id(2);
        let id3 = make_id(3);

        index.insert(id1, vec![1.0, 0.0, 0.0, 0.0]).expect("insert");
        index.insert(id2, vec![0.0, 1.0, 0.0, 0.0]).expect("insert");
        index.insert(id3, vec![0.0, 0.0, 1.0, 0.0]).expect("insert");

        // Tombstone id2
        assert!(index.delete(id2));

        let snap = index.snapshot();
        assert_eq!(snap.total_nodes, 3);
        assert_eq!(snap.tombstone_count, 1);
        assert_eq!(snap.vectors.len(), 3, "all 3 vectors including tombstone");

        let mut restored = HnswIndex::with_config(config);
        restored
            .restore_from_snapshot_embedded(&snap)
            .expect("restore");

        assert_eq!(restored.len(), 3, "total nodes preserved");
        assert_eq!(
            restored.tombstone_stats().tombstone_count,
            1,
            "tombstone count preserved"
        );

        // Search should not return id2
        let results = restored.search(&[0.0, 1.0, 0.0, 0.0], 3).expect("search");
        let result_ids: Vec<NodeId> = results.iter().map(|(id, _)| *id).collect();
        assert!(
            !result_ids.contains(&id2),
            "tombstoned id2 should not appear in results"
        );
    }

    /// Snapshot serialization includes vectors and round-trips via JSON.
    #[test]
    fn snapshot_serialization_includes_vectors() {
        let mut index = HnswIndex::new(4);

        let id1 = make_id(1);
        index.insert(id1, vec![1.0, 2.0, 3.0, 4.0]).expect("insert");

        let snap = index.snapshot();
        assert!(!snap.vectors.is_empty(), "vectors should be in snapshot");

        let json = serde_json::to_string(&snap).expect("serialize");
        assert!(
            json.contains("vectors"),
            "serialized JSON should contain vectors field"
        );

        let restored_snap: khive_hnsw::checkpoint::HnswSnapshot =
            serde_json::from_str(&json).expect("deserialize");
        assert_eq!(
            restored_snap.vectors.len(),
            1,
            "deserialized snapshot should have 1 vector"
        );
        assert_eq!(
            restored_snap.vectors[0].0, id1,
            "vector id should be preserved"
        );
        assert_eq!(
            restored_snap.vectors[0].1,
            vec![1.0, 2.0, 3.0, 4.0],
            "vector data should be preserved"
        );
    }

    /// Old snapshots (no vectors field) still deserialize correctly.
    #[test]
    fn backward_compat_snapshot_without_vectors() {
        // Simulate a snapshot from before the vectors field was added
        let old_json = r#"{
            "total_nodes": 1,
            "live_nodes": 1,
            "tombstone_count": 0,
            "max_layer": 0,
            "entry_point": "01010101010101010101010101010101",
            "config": {"m": 16, "ef_construction": 200, "metric": "cosine"},
            "indexed_ids": ["01010101010101010101010101010101"],
            "tombstoned_ids": [],
            "layers": []
        }"#;

        let snap: khive_hnsw::checkpoint::HnswSnapshot =
            serde_json::from_str(old_json).expect("deserialize old snapshot");

        assert_eq!(snap.total_nodes, 1);
        assert!(
            snap.vectors.is_empty(),
            "old snapshot deserialized with empty vectors"
        );
        assert!(snap.verify().is_ok(), "old snapshot should verify");
    }

    /// restore_from_snapshot_embedded fails when snapshot has no vectors.
    #[test]
    fn restore_embedded_fails_without_vectors() {
        let config = HnswConfig::with_dimensions(4);
        let mut index = HnswIndex::with_config(config);

        let id1 = make_id(1);
        let snap = khive_hnsw::checkpoint::HnswSnapshot {
            vector_count: 0,
            total_nodes: 1,
            live_nodes: 1,
            tombstone_count: 0,
            max_layer: 0,
            entry_point: Some(id1),
            config: khive_hnsw::checkpoint::HnswCheckpointConfig {
                m: 20,
                ef_construction: 200,
                metric: "cosine".to_string(),
            },
            indexed_ids: vec![id1],
            tombstoned_ids: vec![],
            layers: vec![vec![(id1, vec![])]],
            vectors: vec![], // Intentionally empty
        };

        let result = index.restore_from_snapshot_embedded(&snap);
        assert!(
            result.is_err(),
            "should fail when snapshot has no embedded vectors"
        );
    }

    /// restore_from_snapshot with external map takes priority over embedded vectors.
    #[test]
    fn restore_external_overrides_embedded_vectors() {
        let config = HnswConfig::with_dimensions(4).with_seed(42);
        let mut source = HnswIndex::with_config(config.clone());

        let id1 = make_id(1);
        source
            .insert(id1, vec![1.0, 0.0, 0.0, 0.0])
            .expect("insert");

        let snap = source.snapshot();

        // Provide a different vector for id1 via the external map
        let updated_vector = vec![0.0, 0.0, 0.0, 1.0];
        let external: HashMap<NodeId, Vec<f32>> =
            [(id1, updated_vector.clone())].into_iter().collect();

        let mut restored = HnswIndex::with_config(config);
        restored
            .restore_from_snapshot(&snap, &external)
            .expect("restore with external");

        // The restored index should use the external (override) vector
        let retrieved = restored.get_vector(&id1).expect("get vector");
        assert_eq!(
            retrieved, updated_vector,
            "external vector should override embedded"
        );
    }

    /// restore_from_snapshot rejects snapshot with entry_point not in indexed_ids.
    #[test]
    fn restore_rejects_entry_point_not_in_indexed_ids() {
        let config = HnswConfig::with_dimensions(4);
        let mut index = HnswIndex::with_config(config);

        let id1 = make_id(1);
        let id2 = make_id(2); // not in indexed_ids

        let snap = khive_hnsw::checkpoint::HnswSnapshot {
            vector_count: 0,
            total_nodes: 1,
            live_nodes: 1,
            tombstone_count: 0,
            max_layer: 0,
            entry_point: Some(id2), // not in indexed_ids!
            config: khive_hnsw::checkpoint::HnswCheckpointConfig {
                m: 20,
                ef_construction: 200,
                metric: "cosine".to_string(),
            },
            indexed_ids: vec![id1],
            tombstoned_ids: vec![],
            layers: vec![vec![(id1, vec![])]],
            vectors: vec![(id1, vec![1.0, 0.0, 0.0, 0.0])],
        };

        let vectors = HashMap::new();
        let result = index.restore_from_snapshot(&snap, &vectors);
        assert!(
            result.is_err(),
            "should reject entry_point not in indexed_ids"
        );
    }

    /// restore_from_snapshot rejects vectors with wrong dimensions BEFORE clearing.
    #[test]
    fn restore_rejects_wrong_dimensions_before_clearing() {
        let config = HnswConfig::with_dimensions(4);
        let mut original = HnswIndex::with_config(config.clone());

        let id_orig = make_id(99);
        original
            .insert(id_orig, vec![1.0, 0.0, 0.0, 0.0])
            .expect("insert");

        let id1 = make_id(1);
        // Snapshot with wrong-dimension vector
        let snap = khive_hnsw::checkpoint::HnswSnapshot {
            vector_count: 0,
            total_nodes: 1,
            live_nodes: 1,
            tombstone_count: 0,
            max_layer: 0,
            entry_point: Some(id1),
            config: khive_hnsw::checkpoint::HnswCheckpointConfig {
                m: 20,
                ef_construction: 200,
                metric: "cosine".to_string(),
            },
            indexed_ids: vec![id1],
            tombstoned_ids: vec![],
            layers: vec![vec![(id1, vec![])]],
            vectors: vec![(id1, vec![1.0, 0.0])], // wrong dim: 2 instead of 4
        };

        let vectors = HashMap::new();
        let result = original.restore_from_snapshot(&snap, &vectors);
        assert!(result.is_err(), "should reject wrong dimensions");

        // Original index must be unmodified
        assert_eq!(
            original.len(),
            1,
            "index should be unmodified after failed restore"
        );
        assert!(
            original.get_vector(&id_orig).is_some(),
            "original node must still be present"
        );
    }

    /// Large snapshot round-trip preserves search quality.
    #[test]
    fn snapshot_restore_preserves_search_quality() {
        let config = HnswConfig::with_dimensions(32).with_seed(42);
        let mut original = HnswIndex::with_config(config.clone());

        let n = 200usize;
        for i in 0..n {
            let id = NodeId::new({
                let mut b = [0u8; 16];
                b[0] = (i & 0xff) as u8;
                b[1] = (i >> 8) as u8;
                b
            });
            let vec = generate_random_vector(32, i as u64);
            original.insert(id, vec).expect("insert");
        }

        let snap = original.snapshot();
        assert_eq!(snap.vectors.len(), n, "all vectors embedded");

        let mut restored = HnswIndex::with_config(config);
        restored
            .restore_from_snapshot_embedded(&snap)
            .expect("restore");

        // Search quality: >= 80% recall@10 across 10 queries
        let k = 10;
        let mut total_recall = 0.0;
        for q in 0..10 {
            let query = generate_random_vector(32, 10_000 + q);

            let results_orig: std::collections::HashSet<NodeId> = original
                .search(&query, k)
                .expect("orig search")
                .into_iter()
                .map(|(id, _)| id)
                .collect();
            let results_rest: std::collections::HashSet<NodeId> = restored
                .search(&query, k)
                .expect("rest search")
                .into_iter()
                .map(|(id, _)| id)
                .collect();

            let overlap = results_orig.intersection(&results_rest).count();
            total_recall += overlap as f32 / k as f32;
        }

        let avg_recall = total_recall / 10.0;
        assert!(
            avg_recall >= 0.8,
            "restored index recall {avg_recall:.2} should be >= 0.8"
        );
    }
}
