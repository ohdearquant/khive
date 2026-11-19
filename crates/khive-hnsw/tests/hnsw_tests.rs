//! Integration tests for khive-hnsw.

#[cfg(test)]
#[path = "hnsw_tests/unit_tests.rs"]
mod unit_tests;

#[cfg(test)]
#[path = "hnsw_tests/memory_budget_tests.rs"]
mod memory_budget_tests;

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
#[path = "hnsw_tests/metrics_tests.rs"]
mod metrics_tests;

#[cfg(test)]
#[path = "hnsw_tests/search_context_tests.rs"]
mod search_context_tests;

// =============================================================================
// Snapshot / restore tests
// =============================================================================

#[cfg(test)]
#[path = "hnsw_tests/snapshot_tests.rs"]
mod snapshot_tests;
