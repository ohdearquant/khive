use criterion::{black_box, criterion_group, criterion_main, BatchSize, BenchmarkGroup, Criterion};
use khive_hnsw::{DistanceMetric, HnswConfig, HnswIndex, HnswSearchContext, NodeId};
use rand::{rngs::StdRng, Rng, SeedableRng};

const DIMS: usize = 384;
const SEED: u64 = 42;

fn random_unit_vector(rng: &mut StdRng) -> Vec<f32> {
    let raw: Vec<f32> = (0..DIMS).map(|_| rng.gen_range(-1.0f32..1.0f32)).collect();
    let norm = raw.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 1e-10 {
        raw.iter().map(|x| x / norm).collect()
    } else {
        raw
    }
}

fn make_node_id(i: usize) -> NodeId {
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&(i as u64).to_le_bytes());
    NodeId::new(bytes)
}

fn build_index(n: usize, config: HnswConfig) -> HnswIndex {
    let mut rng = StdRng::seed_from_u64(SEED);
    let mut index = HnswIndex::with_config(config);
    for i in 0..n {
        let v = random_unit_vector(&mut rng);
        index.insert(make_node_id(i), v).unwrap();
    }
    index
}

fn bench_build(c: &mut Criterion) {
    let mut group: BenchmarkGroup<_> = c.benchmark_group("build");
    group.sample_size(10);

    // Sequential insert: vectors pre-generated outside the timed loop so only
    // index construction time is measured.
    for &n in &[1_000usize, 5_000] {
        let config = HnswConfig {
            seed: Some(SEED),
            ..HnswConfig::with_dimensions(DIMS)
        };
        let vectors: Vec<Vec<f32>> = {
            let mut rng = StdRng::seed_from_u64(SEED);
            (0..n).map(|_| random_unit_vector(&mut rng)).collect()
        };
        group.bench_function(format!("sequential_{n}"), |b| {
            b.iter_batched(
                || (HnswIndex::with_config(config.clone()), vectors.clone()),
                |(mut index, vecs)| {
                    for (i, v) in vecs.into_iter().enumerate() {
                        index.insert(make_node_id(i), v).unwrap();
                    }
                    black_box(index.len())
                },
                BatchSize::SmallInput,
            )
        });
    }

    group.finish();
}

fn bench_search(c: &mut Criterion) {
    let mut group: BenchmarkGroup<_> = c.benchmark_group("search");
    group.sample_size(50);

    let config_5k = HnswConfig {
        seed: Some(SEED),
        ..HnswConfig::with_dimensions(DIMS)
    };
    let index_5k = build_index(5_000, config_5k);

    let mut query_rng = StdRng::seed_from_u64(SEED + 1);
    let queries: Vec<Vec<f32>> = (0..20)
        .map(|_| random_unit_vector(&mut query_rng))
        .collect();

    for &k in &[10usize, 50] {
        let index_ref = &index_5k;
        let queries_ref = &queries;
        group.bench_function(format!("n5k_k{k}"), |b| {
            let mut qi = 0usize;
            b.iter(|| {
                let q = &queries_ref[qi % queries_ref.len()];
                qi += 1;
                black_box(index_ref.search(black_box(q), k).unwrap())
            })
        });
    }

    for &k in &[10usize, 50] {
        let index_ref = &index_5k;
        let queries_ref = &queries;
        let ef = index_5k.config().ef_search;
        group.bench_function(format!("n5k_k{k}_with_ctx"), |b| {
            let mut ctx = HnswSearchContext::new(ef);
            let mut qi = 0usize;
            b.iter(|| {
                let q = &queries_ref[qi % queries_ref.len()];
                qi += 1;
                black_box(
                    index_ref
                        .search_with_context(black_box(q), k, &mut ctx)
                        .unwrap(),
                )
            })
        });
    }

    group.finish();
}

fn bench_search_quantized(c: &mut Criterion) {
    let mut group: BenchmarkGroup<_> = c.benchmark_group("search_quantized");
    group.sample_size(50);

    let config = HnswConfig {
        seed: Some(SEED),
        ..HnswConfig::with_dimensions(DIMS)
    };

    let mut rng = StdRng::seed_from_u64(SEED);
    let mut index = HnswIndex::with_config(config).with_quantized();
    for i in 0..5_000 {
        let v = random_unit_vector(&mut rng);
        index.insert(make_node_id(i), v).unwrap();
    }

    let mut query_rng = StdRng::seed_from_u64(SEED + 1);
    let queries: Vec<Vec<f32>> = (0..20)
        .map(|_| random_unit_vector(&mut query_rng))
        .collect();

    let index_ref = &index;
    let queries_ref = &queries;
    group.bench_function("n5k_k10_int8", |b| {
        let mut qi = 0usize;
        b.iter(|| {
            let q = &queries_ref[qi % queries_ref.len()];
            qi += 1;
            black_box(index_ref.search(black_box(q), 10).unwrap())
        })
    });

    group.finish();
}

fn bench_distance(c: &mut Criterion) {
    let mut group: BenchmarkGroup<_> = c.benchmark_group("distance");
    group.sample_size(200);

    let mut rng = StdRng::seed_from_u64(SEED);
    let a: Vec<f32> = (0..DIMS).map(|_| rng.gen_range(-1.0f32..1.0f32)).collect();
    let b: Vec<f32> = (0..DIMS).map(|_| rng.gen_range(-1.0f32..1.0f32)).collect();

    group.bench_function("cosine_384d", |b_bench| {
        let a_ref = &a;
        let b_ref = &b;
        b_bench.iter(|| {
            let dot = lattice_embed::simd::dot_product(black_box(a_ref), black_box(b_ref));
            let a_norm = a_ref.iter().map(|x| x * x).sum::<f32>().sqrt();
            let b_norm = b_ref.iter().map(|x| x * x).sum::<f32>().sqrt();
            let denom = a_norm * b_norm;
            if denom > 0.0 {
                black_box(1.0 - (dot / denom).clamp(-1.0, 1.0))
            } else {
                black_box(1.0f32)
            }
        })
    });

    group.bench_function("l2_384d", |b_bench| {
        let a_ref = &a;
        let b_ref = &b;
        b_bench.iter(|| {
            black_box(lattice_embed::simd::euclidean_distance(
                black_box(a_ref),
                black_box(b_ref),
            ))
        })
    });

    group.bench_function("dot_384d", |b_bench| {
        let a_ref = &a;
        let b_ref = &b;
        b_bench.iter(|| {
            black_box(lattice_embed::simd::dot_product(
                black_box(a_ref),
                black_box(b_ref),
            ))
        })
    });

    group.finish();
}

fn bench_search_context_alloc(c: &mut Criterion) {
    let mut group: BenchmarkGroup<_> = c.benchmark_group("search_context");
    group.sample_size(200);

    group.bench_function("new_ef80", |b| {
        b.iter(|| black_box(HnswSearchContext::new(80)))
    });

    group.bench_function("new_ef200", |b| {
        b.iter(|| black_box(HnswSearchContext::new(200)))
    });

    let config_5k = HnswConfig {
        seed: Some(SEED),
        ..HnswConfig::with_dimensions(DIMS)
    };
    let index_5k = build_index(5_000, config_5k);
    let mut query_rng = StdRng::seed_from_u64(SEED + 1);
    let queries: Vec<Vec<f32>> = (0..20)
        .map(|_| random_unit_vector(&mut query_rng))
        .collect();
    let ef = index_5k.config().ef_search;

    group.bench_function("ctx_reuse_k10", |b| {
        let mut ctx = HnswSearchContext::new(ef);
        let mut qi = 0usize;
        b.iter(|| {
            let q = &queries[qi % queries.len()];
            qi += 1;
            black_box(
                index_5k
                    .search_with_context(black_box(q), 10, &mut ctx)
                    .unwrap(),
            )
        })
    });

    group.bench_function("ctx_fresh_k10", |b| {
        let mut qi = 0usize;
        b.iter(|| {
            let q = &queries[qi % queries.len()];
            qi += 1;
            let mut ctx = HnswSearchContext::new(ef);
            black_box(
                index_5k
                    .search_with_context(black_box(q), 10, &mut ctx)
                    .unwrap(),
            )
        })
    });

    group.finish();
}

fn bench_search_metrics(c: &mut Criterion) {
    let mut group: BenchmarkGroup<_> = c.benchmark_group("search_metrics");
    group.sample_size(50);

    let config_l2 = HnswConfig {
        seed: Some(SEED),
        metric: DistanceMetric::L2,
        ..HnswConfig::with_dimensions(DIMS)
    };
    let index_l2 = build_index(5_000, config_l2);

    let config_cosine = HnswConfig {
        seed: Some(SEED),
        metric: DistanceMetric::Cosine,
        ..HnswConfig::with_dimensions(DIMS)
    };
    let index_cosine = build_index(5_000, config_cosine);

    let config_dot = HnswConfig {
        seed: Some(SEED),
        metric: DistanceMetric::Dot,
        ..HnswConfig::with_dimensions(DIMS)
    };
    let index_dot = build_index(5_000, config_dot);

    let mut query_rng = StdRng::seed_from_u64(SEED + 1);
    let queries: Vec<Vec<f32>> = (0..20)
        .map(|_| random_unit_vector(&mut query_rng))
        .collect();

    for (name, index) in &[
        ("cosine", &index_cosine),
        ("l2", &index_l2),
        ("dot", &index_dot),
    ] {
        let queries_ref = &queries;
        group.bench_function(format!("n5k_k10_{name}"), |b| {
            let mut qi = 0usize;
            b.iter(|| {
                let q = &queries_ref[qi % queries_ref.len()];
                qi += 1;
                black_box(index.search(black_box(q), 10).unwrap())
            })
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_build,
    bench_search,
    bench_search_quantized,
    bench_distance,
    bench_search_context_alloc,
    bench_search_metrics,
);
criterion_main!(benches);
