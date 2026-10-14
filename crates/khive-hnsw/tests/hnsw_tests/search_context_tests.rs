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
        for (i, (r_normal, r_ctx)) in results_normal.iter().zip(results_ctx.iter()).enumerate() {
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
