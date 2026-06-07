use criterion::{black_box, criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion};
use khive_score::{
    avg_scores, cmp_desc_then_id, max_score, min_score, rrf_score, score_from_distance_lossy,
    sum_scores, weighted_sum, DeterministicScore, Ranked,
};
use khive_types::DistanceMetric;

fn make_scores(n: usize, seed_offset: u64) -> Vec<DeterministicScore> {
    // Deterministic pseudo-random values in [0.0, 1.0) using a simple LCG.
    // No rand dependency needed — avoids any version skew.
    let mut state = 42u64.wrapping_add(seed_offset);
    (0..n)
        .map(|_| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let frac = (state >> 32) as f64 / u32::MAX as f64;
            DeterministicScore::from_f64(frac)
        })
        .collect()
}

fn make_f32_distances(n: usize) -> Vec<f32> {
    let mut state = 0xdead_beef_u64;
    (0..n)
        .map(|_| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            // cosine distance lives in [0.0, 2.0]; use [0.0, 1.5]
            (state >> 32) as f32 / u32::MAX as f32 * 1.5
        })
        .collect()
}

// ── DeterministicScore construction / conversion ──────────────────────────────

fn bench_from_f64(c: &mut Criterion) {
    let mut g = c.benchmark_group("DeterministicScore/from_f64");
    g.sample_size(200);

    g.bench_function("scalar", |b| {
        b.iter(|| DeterministicScore::from_f64(black_box(0.73819)))
    });

    let vals: Vec<f64> = (0..1000).map(|i| i as f64 / 1000.0).collect();
    g.bench_function("batch_1000", |b| {
        b.iter(|| {
            vals.iter()
                .map(|&v| DeterministicScore::from_f64(black_box(v)))
                .fold(DeterministicScore::ZERO, |acc, s| acc + s)
        })
    });

    g.bench_function("nan", |b| {
        b.iter(|| DeterministicScore::from_f64(black_box(f64::NAN)))
    });

    g.bench_function("pos_inf", |b| {
        b.iter(|| DeterministicScore::from_f64(black_box(f64::INFINITY)))
    });

    g.finish();
}

fn bench_from_f32(c: &mut Criterion) {
    let mut g = c.benchmark_group("DeterministicScore/from_f32");
    g.sample_size(200);

    g.bench_function("scalar", |b| {
        b.iter(|| DeterministicScore::from_f32(black_box(0.73819_f32)))
    });

    let vals: Vec<f32> = (0..1000).map(|i| i as f32 / 1000.0).collect();
    g.bench_function("batch_1000", |b| {
        b.iter(|| {
            vals.iter()
                .map(|&v| DeterministicScore::from_f32(black_box(v)))
                .fold(DeterministicScore::ZERO, |acc, s| acc + s)
        })
    });

    g.finish();
}

fn bench_to_f64(c: &mut Criterion) {
    let mut g = c.benchmark_group("DeterministicScore/to_f64");
    g.sample_size(200);

    let score = DeterministicScore::from_f64(0.73819);
    g.bench_function("scalar", |b| b.iter(|| black_box(score).to_f64()));

    let scores = make_scores(1000, 0);
    g.bench_function("batch_1000", |b| {
        b.iter(|| scores.iter().map(|s| black_box(*s).to_f64()).sum::<f64>())
    });

    g.finish();
}

// ── Aggregation ops ───────────────────────────────────────────────────────────

fn bench_sum_scores(c: &mut Criterion) {
    let mut g = c.benchmark_group("ops/sum_scores");
    g.sample_size(100);

    for n in [10usize, 100, 1000] {
        let scores = make_scores(n, 1);
        g.bench_with_input(BenchmarkId::from_parameter(n), &scores, |b, s| {
            b.iter(|| sum_scores(black_box(s)))
        });
    }

    g.finish();
}

fn bench_avg_scores(c: &mut Criterion) {
    let mut g = c.benchmark_group("ops/avg_scores");
    g.sample_size(100);

    for n in [10usize, 100, 1000] {
        let scores = make_scores(n, 2);
        g.bench_with_input(BenchmarkId::from_parameter(n), &scores, |b, s| {
            b.iter(|| avg_scores(black_box(s)))
        });
    }

    g.finish();
}

fn bench_max_min(c: &mut Criterion) {
    let mut g = c.benchmark_group("ops/max_min");
    g.sample_size(100);

    let scores_1000 = make_scores(1000, 3);
    g.bench_function("max_1000", |b| {
        b.iter(|| max_score(black_box(&scores_1000)))
    });
    g.bench_function("min_1000", |b| {
        b.iter(|| min_score(black_box(&scores_1000)))
    });

    g.finish();
}

fn bench_rrf_score(c: &mut Criterion) {
    let mut g = c.benchmark_group("ops/rrf_score");
    g.sample_size(200);

    g.bench_function("rank_1_k60", |b| {
        b.iter(|| rrf_score(black_box(1), black_box(60)))
    });

    // Simulates building an RRF-fused result set for 1000 candidates.
    g.bench_function("batch_1000_k60", |b| {
        b.iter(|| {
            (1usize..=1000)
                .map(|rank| rrf_score(black_box(rank), 60))
                .fold(DeterministicScore::ZERO, |acc, s| acc + s)
        })
    });

    g.finish();
}

fn bench_weighted_sum(c: &mut Criterion) {
    let mut g = c.benchmark_group("ops/weighted_sum");
    g.sample_size(100);

    for n in [2usize, 8, 32] {
        let scores = make_scores(n, 4);
        let weights: Vec<f64> = (0..n).map(|i| (i + 1) as f64 / n as f64).collect();
        g.bench_with_input(
            BenchmarkId::from_parameter(n),
            &(scores, weights),
            |b, (s, w)| b.iter(|| weighted_sum(black_box(s), black_box(w))),
        );
    }

    g.finish();
}

// ── score_from_distance ───────────────────────────────────────────────────────

fn bench_score_from_distance(c: &mut Criterion) {
    let mut g = c.benchmark_group("score_from_distance");
    g.sample_size(200);

    g.bench_function("cosine_scalar", |b| {
        b.iter(|| score_from_distance_lossy(black_box(0.35_f32), DistanceMetric::Cosine))
    });

    g.bench_function("l2_scalar", |b| {
        b.iter(|| score_from_distance_lossy(black_box(1.2_f32), DistanceMetric::L2))
    });

    g.bench_function("dot_scalar", |b| {
        b.iter(|| score_from_distance_lossy(black_box(-3.7_f32), DistanceMetric::Dot))
    });

    let dists = make_f32_distances(1000);

    g.bench_function("cosine_batch_1000", |b| {
        b.iter(|| {
            dists
                .iter()
                .map(|&d| score_from_distance_lossy(black_box(d), DistanceMetric::Cosine))
                .fold(DeterministicScore::ZERO, |acc, s| acc + s)
        })
    });

    g.bench_function("l2_batch_1000", |b| {
        b.iter(|| {
            dists
                .iter()
                .map(|&d| score_from_distance_lossy(black_box(d), DistanceMetric::L2))
                .fold(DeterministicScore::ZERO, |acc, s| acc + s)
        })
    });

    g.bench_function("dot_batch_1000", |b| {
        b.iter(|| {
            dists
                .iter()
                .map(|&d| score_from_distance_lossy(black_box(d), DistanceMetric::Dot))
                .fold(DeterministicScore::ZERO, |acc, s| acc + s)
        })
    });

    g.finish();
}

// ── Comparator / sorting ──────────────────────────────────────────────────────

fn bench_cmp_desc_then_id(c: &mut Criterion) {
    let mut g = c.benchmark_group("comparator");
    g.sample_size(100);

    // Single comparison: the hot path in a heap push.
    let sa = DeterministicScore::from_f64(0.9);
    let sb = DeterministicScore::from_f64(0.7);
    g.bench_function("cmp_desc_scalar", |b| {
        b.iter(|| {
            cmp_desc_then_id(
                black_box(sa),
                black_box(&1u64),
                black_box(sb),
                black_box(&2u64),
            )
        })
    });

    // sort_unstable on 1000 (DeterministicScore, u64) pairs — mimics per-recall sort.
    // Use iter_batched to clone the unsorted base each iteration, so we always
    // measure sorting a random input rather than an already-sorted vector.
    let pairs_base: Vec<(DeterministicScore, u64)> = make_scores(1000, 5)
        .into_iter()
        .enumerate()
        .map(|(i, s)| (s, i as u64))
        .collect();

    g.bench_function("sort_1000_pairs", |b| {
        b.iter_batched(
            || pairs_base.clone(),
            |mut pairs| {
                pairs.sort_unstable_by(|(sa, ia), (sb, ib)| cmp_desc_then_id(*sa, ia, *sb, ib));
                black_box(pairs);
            },
            BatchSize::SmallInput,
        )
    });

    // Ranked<u64> heap: simulates top-k extraction via BinaryHeap.
    let ranked: Vec<Ranked<u64>> = make_scores(1000, 6)
        .into_iter()
        .enumerate()
        .map(|(i, s)| Ranked::new(s, i as u64))
        .collect();

    g.bench_function("ranked_heap_1000", |b| {
        b.iter(|| {
            use std::collections::BinaryHeap;
            let mut heap: BinaryHeap<Ranked<u64>> = ranked.iter().cloned().collect();
            let top10: Vec<_> = (0..10).filter_map(|_| heap.pop()).collect();
            black_box(top10);
        })
    });

    g.finish();
}

// ── Arithmetic operators ──────────────────────────────────────────────────────

fn bench_arithmetic(c: &mut Criterion) {
    let mut g = c.benchmark_group("DeterministicScore/arithmetic");
    g.sample_size(200);

    let a = DeterministicScore::from_f64(0.6);
    let b = DeterministicScore::from_f64(0.4);

    g.bench_function("add", |b_| b_.iter(|| black_box(a) + black_box(b)));
    g.bench_function("sub", |b_| b_.iter(|| black_box(a) - black_box(b)));
    g.bench_function("mul_i64", |b_| b_.iter(|| black_box(a) * black_box(2i64)));
    g.bench_function("mul_f64", |b_| b_.iter(|| black_box(a) * black_box(0.5f64)));
    g.bench_function("div_i64", |b_| b_.iter(|| black_box(a) / black_box(3i64)));

    g.finish();
}

criterion_group!(
    benches,
    bench_from_f64,
    bench_from_f32,
    bench_to_f64,
    bench_sum_scores,
    bench_avg_scores,
    bench_max_min,
    bench_rrf_score,
    bench_weighted_sum,
    bench_score_from_distance,
    bench_cmp_desc_then_id,
    bench_arithmetic,
);
criterion_main!(benches);
