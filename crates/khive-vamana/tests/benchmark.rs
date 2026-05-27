use std::time::Instant;

use khive_vamana::{VamanaConfig, VamanaIndex};
use rand::{prelude::*, SeedableRng};

fn rand_unit_vectors(n: usize, dim: usize, seed: u64) -> Vec<f32> {
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
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

#[test]
fn integration_random_1000x384_recall_at_10_at_least_80_percent() {
    let n = 1000usize;
    let dim = 384usize;
    let k = 10usize;

    let vectors = rand_unit_vectors(n, dim, 0xDEAD_BEEF);
    let cfg = VamanaConfig::with_dimensions(dim);
    let index = VamanaIndex::build(&vectors, cfg).expect("build failed");

    let queries = rand_unit_vectors(50, dim, 0xCAFE_BABE);
    let recall = index.recall_at_k(&queries, k).expect("recall failed");

    assert!(recall >= 0.80, "recall@10 {recall:.4} < 0.80 for 1000×384");
}

#[test]
#[ignore] // ~60s on CI; run with `cargo test --ignored`
fn benchmark_random_5000x384_recall_at_10_at_least_85_percent() {
    let n = 5000usize;
    let dim = 384usize;
    let k = 10usize;
    let num_queries = 100usize;

    let vectors = rand_unit_vectors(n, dim, 0xFEED_FACE);
    let cfg = VamanaConfig::with_dimensions(dim);

    let t_build = Instant::now();
    let index = VamanaIndex::build(&vectors, cfg).expect("build failed");
    let build_time_ms = t_build.elapsed().as_millis();

    let queries = rand_unit_vectors(num_queries, dim, 0xABCD_1234);

    let t_total = Instant::now();
    for qi in 0..num_queries {
        let q = &queries[qi * dim..(qi + 1) * dim];
        let _ = index.search(q, k).expect("search failed");
    }
    let total_query_us = t_total.elapsed().as_micros();
    let average_query_latency_us = total_query_us / num_queries as u128;
    let single_query_latency_us = {
        let q = &queries[..dim];
        let t = Instant::now();
        let _ = index.search(q, k).expect("search failed");
        t.elapsed().as_micros()
    };

    let recall = index.recall_at_k(&queries, k).expect("recall failed");

    // Print under --nocapture; deterministic values for debugging
    println!("build_time_ms: {build_time_ms}");
    println!("single_query_latency_us: {single_query_latency_us}");
    println!("average_query_latency_us_100: {average_query_latency_us}");
    println!("recall_at_10: {recall:.4}");

    assert!(recall >= 0.85, "recall@10 {recall:.4} < 0.85 for 5000×384");
}
