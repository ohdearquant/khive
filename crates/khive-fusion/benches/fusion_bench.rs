use criterion::{
    black_box, criterion_group, criterion_main, measurement::WallTime, BenchmarkGroup, BenchmarkId,
    Criterion,
};
use khive_fusion::{
    fuse, normalize_weights, reciprocal_rank_fusion, union_fusion, weighted_fusion,
    weights_are_normalized, FusionStrategy,
};
use khive_score::DeterministicScore;

fn make_source(n: usize, seed_offset: u64) -> Vec<(u64, DeterministicScore)> {
    // Deterministic LCG with seed 42 + offset
    let mut state: u64 = 42u64
        .wrapping_add(seed_offset)
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    (0..n)
        .map(|i| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let score_bits = (state >> 33) as f64 / (u32::MAX as f64);
            (i as u64, DeterministicScore::from_f64(score_bits))
        })
        .collect()
}

// Build N sources where each has `per_source` items; 30% of IDs overlap across sources
// to exercise the hash-map merge path that matters for real hybrid search.
fn make_sources(num_sources: usize, per_source: usize) -> Vec<Vec<(u64, DeterministicScore)>> {
    let overlap = per_source / 3;
    (0..num_sources)
        .map(|s| {
            // First `overlap` IDs are shared across all sources (0..overlap)
            // Rest are source-private (s * per_source + overlap .. )
            let shared: Vec<(u64, DeterministicScore)> = (0..overlap)
                .map(|i| {
                    let mut state = (42u64 ^ (s as u64 * 997))
                        .wrapping_mul(6364136223846793005)
                        .wrapping_add(1442695040888963407);
                    state = state
                        .wrapping_add(i as u64)
                        .wrapping_mul(6364136223846793005);
                    let score = (state >> 33) as f64 / (u32::MAX as f64);
                    (i as u64, DeterministicScore::from_f64(score))
                })
                .collect();

            let private: Vec<(u64, DeterministicScore)> =
                make_source(per_source - overlap, (s as u64 + 1) * 1000)
                    .into_iter()
                    .map(|(i, score)| (i + (s as u64 * per_source as u64) + 1_000_000, score))
                    .collect();

            let mut source = shared;
            source.extend(private);
            source
        })
        .collect()
}

fn bench_rrf(c: &mut Criterion) {
    let mut group: BenchmarkGroup<WallTime> = c.benchmark_group("rrf");
    group.sample_size(50);

    for (n_sources, per_source) in [(2, 50), (2, 150), (2, 500), (3, 150), (3, 500)] {
        let sources = make_sources(n_sources, per_source);
        let id = BenchmarkId::new(
            format!("{src}src", src = n_sources),
            format!("{n}items", n = per_source),
        );
        group.bench_with_input(id, &sources, |b, srcs| {
            b.iter(|| reciprocal_rank_fusion(black_box(srcs.clone()), black_box(60)));
        });
    }

    group.finish();
}

fn bench_weighted(c: &mut Criterion) {
    let mut group: BenchmarkGroup<WallTime> = c.benchmark_group("weighted");
    group.sample_size(50);

    for (n_sources, per_source) in [(2, 50), (2, 150), (2, 500), (3, 150), (3, 500)] {
        let sources = make_sources(n_sources, per_source);
        let weights: Vec<f64> = (0..n_sources).map(|i| 1.0 / (i + 1) as f64).collect();
        let id = BenchmarkId::new(
            format!("{src}src", src = n_sources),
            format!("{n}items", n = per_source),
        );
        group.bench_with_input(id, &(sources, weights), |b, (srcs, ws)| {
            b.iter(|| weighted_fusion(black_box(srcs.clone()), black_box(ws)));
        });
    }

    group.finish();
}

fn bench_union(c: &mut Criterion) {
    let mut group: BenchmarkGroup<WallTime> = c.benchmark_group("union");
    group.sample_size(50);

    for (n_sources, per_source) in [(2, 50), (2, 150), (2, 500), (3, 150), (3, 500)] {
        let sources = make_sources(n_sources, per_source);
        let id = BenchmarkId::new(
            format!("{src}src", src = n_sources),
            format!("{n}items", n = per_source),
        );
        group.bench_with_input(id, &sources, |b, srcs| {
            b.iter(|| union_fusion(black_box(srcs.clone())));
        });
    }

    group.finish();
}

fn bench_fuse_dispatcher(c: &mut Criterion) {
    let mut group: BenchmarkGroup<WallTime> = c.benchmark_group("fuse_dispatcher");
    group.sample_size(50);

    let sources_150 = make_sources(2, 150);
    let weights = vec![0.6f64, 0.4];

    for strategy in [
        FusionStrategy::Rrf { k: 60 },
        FusionStrategy::Weighted {
            weights: weights.clone(),
        },
        FusionStrategy::Union,
        FusionStrategy::VectorOnly,
        FusionStrategy::KeywordOnly,
    ] {
        let name = format!("{strategy:?}")
            .split_once('{')
            .map(|(n, _)| n.trim().to_string())
            .unwrap_or_else(|| format!("{strategy:?}"));
        group.bench_function(name, |b| {
            b.iter(|| {
                fuse(
                    black_box(sources_150.clone()),
                    black_box(&strategy),
                    black_box(20),
                )
            });
        });
    }

    // top_k sensitivity: same strategy, different limits
    let rrf = FusionStrategy::Rrf { k: 60 };
    let sources_500 = make_sources(2, 500);
    for top_k in [10, 50, 100] {
        group.bench_with_input(BenchmarkId::new("rrf_topk", top_k), &top_k, |b, &k| {
            b.iter(|| {
                fuse(
                    black_box(sources_500.clone()),
                    black_box(&rrf),
                    black_box(k),
                )
            });
        });
    }

    group.finish();
}

fn bench_weight_utilities(c: &mut Criterion) {
    let mut group: BenchmarkGroup<WallTime> = c.benchmark_group("weight_utils");
    group.sample_size(200);

    let weights_2 = vec![0.7f64, 0.3];
    let weights_3 = vec![1.0f64, 2.0, 3.0];
    let weights_large: Vec<f64> = (1..=20).map(|i| i as f64).collect();

    group.bench_function("normalize_2", |b| {
        b.iter(|| normalize_weights(black_box(&weights_2)));
    });
    group.bench_function("normalize_3", |b| {
        b.iter(|| normalize_weights(black_box(&weights_3)));
    });
    group.bench_function("normalize_20", |b| {
        b.iter(|| normalize_weights(black_box(&weights_large)));
    });
    group.bench_function("is_normalized_true", |b| {
        b.iter(|| weights_are_normalized(black_box(&weights_2), black_box(1e-6)));
    });
    group.bench_function("is_normalized_false", |b| {
        b.iter(|| weights_are_normalized(black_box(&weights_3), black_box(1e-6)));
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_rrf,
    bench_weighted,
    bench_union,
    bench_fuse_dispatcher,
    bench_weight_utilities,
);
criterion_main!(benches);
