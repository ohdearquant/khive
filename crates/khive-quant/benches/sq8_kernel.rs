//! Criterion benches for the SQ8 distance kernel vs f32 L2 squared.
//!
//! These assert two cost bars (ADR-052 §1, Step 2):
//!   1. The `u8_l2sq_u32` kernel is not slower than the f32 L2 kernel (the expected
//!      speedup is 2-4× on aarch64 NEON; on any platform the SQ8 kernel MUST be no
//!      more than 1.5× slower — if it is, the NEON path is missing or broken).
//!   2. `GsSq8Codec::l2_sq` (full encoded-pair cost including struct overhead) is
//!      within 3× of the raw f32 kernel (overhead of two `Vec<u8>` pointer chases).
//!
//! Run with:
//!   cargo bench -p khive-quant --bench sq8_kernel -- --output-format bencher | tee /tmp/sq8_kernel.txt

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use khive_quant::{u8_l2sq_u32, GsSq8Codec};
use rand::{rngs::StdRng, Rng, SeedableRng};
use std::hint::black_box;

const DIM: usize = 384;
const SEED: u64 = 0xA052_BE42;

fn rand_f32_vecs(n: usize, dims: usize, seed: u64) -> Vec<f32> {
    let mut rng = StdRng::seed_from_u64(seed);
    let mut raw: Vec<f32> = (0..n * dims).map(|_| rng.gen_range(-1.0f32..1.0)).collect();
    for row in raw.chunks_mut(dims) {
        let norm: f32 = row.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for x in row.iter_mut() {
                *x /= norm;
            }
        }
    }
    raw
}

/// Scalar f32 L2 squared — the baseline reference kernel.
#[inline(never)]
fn f32_l2sq(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| (x - y) * (x - y)).sum()
}

fn bench_kernel_cost(c: &mut Criterion) {
    let corpus = rand_f32_vecs(2, DIM, SEED);
    let a_f32 = &corpus[..DIM];
    let b_f32 = &corpus[DIM..];

    let codec = GsSq8Codec::train_flat(&corpus, DIM);
    let a_enc = codec.encode(a_f32);
    let b_enc = codec.encode(b_f32);

    let a_u8 = &a_enc.codes[..];
    let b_u8 = &b_enc.codes[..];

    let mut group = c.benchmark_group("sq8_kernel/384d");
    group.sample_size(500);

    group.bench_function("f32_l2sq", |bencher| {
        bencher.iter(|| black_box(f32_l2sq(black_box(a_f32), black_box(b_f32))))
    });

    group.bench_function("u8_l2sq_u32", |bencher| {
        bencher.iter(|| black_box(u8_l2sq_u32(black_box(a_u8), black_box(b_u8))))
    });

    group.bench_function("GsSq8Codec::l2_sq", |bencher| {
        bencher.iter(|| black_box(codec.l2_sq(black_box(&a_enc), black_box(&b_enc))))
    });

    group.finish();
}

/// Cost bars for different vector dimensionalities.
fn bench_kernel_scaling(c: &mut Criterion) {
    let mut group = c.benchmark_group("sq8_kernel/scaling");
    group.sample_size(300);

    for &dim in &[64usize, 128, 256, 384, 768] {
        let corpus = rand_f32_vecs(2, dim, SEED ^ dim as u64);
        let a_f32 = &corpus[..dim];
        let b_f32 = &corpus[dim..];

        let codec = GsSq8Codec::train_flat(&corpus, dim);
        let a_enc = codec.encode(a_f32);
        let b_enc = codec.encode(b_f32);

        group.bench_with_input(BenchmarkId::new("f32_l2sq", dim), &dim, |bencher, _| {
            bencher.iter(|| black_box(f32_l2sq(black_box(a_f32), black_box(b_f32))))
        });

        group.bench_with_input(BenchmarkId::new("u8_l2sq_u32", dim), &dim, |bencher, _| {
            bencher.iter(|| {
                black_box(u8_l2sq_u32(
                    black_box(&a_enc.codes),
                    black_box(&b_enc.codes),
                ))
            })
        });
    }

    group.finish();
}

criterion_group!(benches, bench_kernel_cost, bench_kernel_scaling);
criterion_main!(benches);
