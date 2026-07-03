//! Enforces the SQ8 cost bars documented in `benches/sq8_kernel.rs`:
//! raw `u8_l2sq_u32` within 1.5x of f32 L2, `GsSq8Codec::l2_sq` within 3x.
//!
//! The Criterion benchmark stays reporting-only; this test is the pass/fail gate.
//! Non-ignored test asserts the helper logic itself. The ignored test measures the
//! real kernels and only runs on demand (release mode, explicit `--ignored`) since
//! wall-clock ratios are noisy in debug/CI-shared environments.

use std::hint::black_box;
use std::time::Instant;

use khive_quant::{u8_l2sq_u32, GsSq8Codec};
use rand::{rngs::StdRng, Rng, SeedableRng};

const DIM: usize = 384;
const SEED: u64 = 0xA052_BE42;
const RAW_KERNEL_MAX_RATIO: f64 = 1.5;
const CODEC_MAX_RATIO: f64 = 3.0;

fn assert_cost_bars(f32_ns: f64, u8_ns: f64, codec_ns: f64) {
    let raw_ratio = u8_ns / f32_ns.max(1e-3);
    let codec_ratio = codec_ns / f32_ns.max(1e-3);
    assert!(
        raw_ratio <= RAW_KERNEL_MAX_RATIO,
        "u8_l2sq_u32 cost ratio {raw_ratio:.2} exceeds {RAW_KERNEL_MAX_RATIO:.2}"
    );
    assert!(
        codec_ratio <= CODEC_MAX_RATIO,
        "GsSq8Codec::l2_sq cost ratio {codec_ratio:.2} exceeds {CODEC_MAX_RATIO:.2}"
    );
}

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

#[inline(never)]
fn f32_l2sq(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| (x - y) * (x - y)).sum()
}

fn time_ns_per_call<T>(iters: u64, mut f: impl FnMut() -> T) -> f64 {
    for _ in 0..10_000 {
        black_box(f());
    }

    let start = Instant::now();
    for _ in 0..iters {
        black_box(f());
    }
    start.elapsed().as_nanos() as f64 / iters as f64
}

#[test]
fn sq8_cost_bar_assertion_rejects_regression() {
    let raw_over = std::panic::catch_unwind(|| assert_cost_bars(10.0, 16.0, 20.0));
    assert!(raw_over.is_err(), "raw kernel ratio above 1.5x must fail");

    let codec_over = std::panic::catch_unwind(|| assert_cost_bars(10.0, 15.0, 31.0));
    assert!(codec_over.is_err(), "codec ratio above 3x must fail");

    assert_cost_bars(10.0, 15.0, 30.0);
}

#[test]
#[ignore]
fn sq8_kernel_cost_bars_are_enforced() {
    let corpus = rand_f32_vecs(2, DIM, SEED);
    let a_f32 = &corpus[..DIM];
    let b_f32 = &corpus[DIM..];

    let codec = GsSq8Codec::train_flat(&corpus, DIM);
    let a_enc = codec.encode(a_f32);
    let b_enc = codec.encode(b_f32);
    let a_u8 = &a_enc.codes[..];
    let b_u8 = &b_enc.codes[..];

    let iters = 200_000;
    let f32_ns = time_ns_per_call(iters, || f32_l2sq(black_box(a_f32), black_box(b_f32)));
    let u8_ns = time_ns_per_call(iters, || u8_l2sq_u32(black_box(a_u8), black_box(b_u8)));
    let codec_ns = time_ns_per_call(iters, || codec.l2_sq(black_box(&a_enc), black_box(&b_enc)));

    eprintln!("sq8 cost bars: f32={f32_ns:.2}ns u8={u8_ns:.2}ns codec={codec_ns:.2}ns");
    assert_cost_bars(f32_ns, u8_ns, codec_ns);
}
