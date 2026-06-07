use criterion::measurement::WallTime;
use criterion::{criterion_group, criterion_main, BenchmarkGroup, BenchmarkId, Criterion};
use khive_vamana::distance::{cosine_from_l2sq, try_l2_squared};
use khive_vamana::{build, search, CorpusFingerprint, VamanaConfig, VamanaIndex, VamanaSnapshot};
use rand::{rngs::StdRng, Rng, SeedableRng};
use std::hint::black_box;

const DIM: usize = 384;
const SEED: u64 = 42;

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

fn make_config(n: usize) -> VamanaConfig {
    // search_list_size must be >= max_degree; use production-realistic settings
    // scaled slightly for bench speed at smaller corpus sizes.
    let (max_degree, search_list_size) = if n <= 1_000 { (32, 64) } else { (64, 128) };
    VamanaConfig::with_dimensions(DIM)
        .with_max_degree(max_degree)
        .with_search_list_size(search_list_size)
}

// ── distance primitives ───────────────────────────────────────────────────────

fn bench_distance(c: &mut Criterion) {
    let mut group: BenchmarkGroup<WallTime> = c.benchmark_group("distance");
    group.sample_size(200);

    let a = rand_unit_vectors(1, DIM, SEED);
    let b = rand_unit_vectors(1, DIM, SEED + 1);

    group.bench_function("l2_squared/384d", |bencher| {
        bencher.iter(|| black_box(try_l2_squared(black_box(&a), black_box(&b)).unwrap()))
    });

    group.bench_function("cosine_from_l2sq", |bencher| {
        let l2sq = try_l2_squared(&a, &b).unwrap();
        bencher.iter(|| black_box(cosine_from_l2sq(black_box(l2sq))))
    });

    group.finish();
}

// ── index construction ────────────────────────────────────────────────────────

fn bench_build(c: &mut Criterion) {
    let mut group: BenchmarkGroup<WallTime> = c.benchmark_group("build");
    group.sample_size(10);

    for &n in &[1_000usize, 5_000, 10_000] {
        let vectors = rand_unit_vectors(n, DIM, SEED);
        let config = make_config(n);

        group.bench_with_input(
            BenchmarkId::new("VamanaIndex::build", n),
            &n,
            |bencher, _| {
                bencher.iter(|| {
                    black_box(
                        VamanaIndex::build(black_box(&vectors), black_box(config.clone()))
                            .expect("build failed"),
                    )
                })
            },
        );
    }

    group.finish();
}

// ── ANN search ───────────────────────────────────────────────────────────────

fn bench_search(c: &mut Criterion) {
    let mut group: BenchmarkGroup<WallTime> = c.benchmark_group("search");
    group.sample_size(50);

    let query = rand_unit_vectors(1, DIM, SEED + 99);

    for &n in &[1_000usize, 5_000, 10_000] {
        let vectors = rand_unit_vectors(n, DIM, SEED);
        let config = make_config(n);
        let index = VamanaIndex::build(&vectors, config).expect("build failed");

        for &k in &[10usize, 50] {
            let id = format!("n={n}/k={k}");
            group.bench_function(&id, |bencher| {
                bencher.iter(|| {
                    black_box(
                        index
                            .search(black_box(&query), black_box(k))
                            .expect("search failed"),
                    )
                })
            });
        }
    }

    group.finish();
}

// ── top-level build+search free functions ────────────────────────────────────

fn bench_free_fns(c: &mut Criterion) {
    let mut group: BenchmarkGroup<WallTime> = c.benchmark_group("free_fns");
    group.sample_size(50);

    let n = 1_000;
    let vectors = rand_unit_vectors(n, DIM, SEED);
    let config = make_config(n);
    let index = build(&vectors, config.clone()).expect("build failed");
    let query = rand_unit_vectors(1, DIM, SEED + 7);

    group.bench_function("build/1k", |bencher| {
        bencher.iter(|| {
            black_box(build(black_box(&vectors), black_box(config.clone())).expect("build failed"))
        })
    });

    group.bench_function("search/1k/k10", |bencher| {
        bencher.iter(|| {
            black_box(
                search(black_box(&index), black_box(&query), black_box(10)).expect("search failed"),
            )
        })
    });

    group.finish();
}

// ── snapshot serialization round-trip ────────────────────────────────────────

fn make_snapshot(index: &VamanaIndex, n: usize) -> VamanaSnapshot {
    let fp = CorpusFingerprint {
        vector_count: n as u64,
        dimensions: DIM as u32,
    };
    let ext_ids: Vec<String> = (0..n).map(|i| format!("id-{i}")).collect();
    index
        .to_snapshot("bench-ns", "bench-model", fp, ext_ids)
        .expect("to_snapshot failed")
}

fn bench_snapshot(c: &mut Criterion) {
    let mut group: BenchmarkGroup<WallTime> = c.benchmark_group("snapshot");
    group.sample_size(50);

    for &n in &[1_000usize, 5_000] {
        let vectors = rand_unit_vectors(n, DIM, SEED);
        let config = make_config(n);
        let index = VamanaIndex::build(&vectors, config).expect("build failed");
        let snapshot = make_snapshot(&index, n);

        group.bench_with_input(BenchmarkId::new("to_snapshot", n), &n, |bencher, &n| {
            let fp = CorpusFingerprint {
                vector_count: n as u64,
                dimensions: DIM as u32,
            };
            bencher.iter_batched(
                || (0..n).map(|i| format!("id-{i}")).collect::<Vec<String>>(),
                |ext_ids| {
                    black_box(
                        index
                            .to_snapshot(
                                "bench-ns",
                                "bench-model",
                                black_box(fp),
                                black_box(ext_ids),
                            )
                            .expect("to_snapshot failed"),
                    )
                },
                criterion::BatchSize::SmallInput,
            )
        });

        group.bench_with_input(
            BenchmarkId::new("from_snapshot", n),
            &snapshot,
            |bencher, snap| {
                bencher.iter(|| {
                    black_box(
                        VamanaIndex::from_snapshot(black_box(snap)).expect("from_snapshot failed"),
                    )
                })
            },
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_distance,
    bench_build,
    bench_search,
    bench_free_fns,
    bench_snapshot,
);
criterion_main!(benches);
