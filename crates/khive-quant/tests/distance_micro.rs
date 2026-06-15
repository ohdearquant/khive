//! Microbenchmark probe: f32 dot vs SQ8 quantized distance — ns/call at 384d.
//! Run with: cargo test -p khive-quant --test distance_micro --release -- --nocapture --ignored

use std::time::Instant;

use khive_quant::{GsSq8Codec, Sq8Codec};
use rand::prelude::*;

/// Generate non-normalized uniform vectors in [-1, 1] using a vetted CSPRNG.
fn gen_uniform_384d(n: usize, dims: usize, seed: u64) -> Vec<Vec<f32>> {
    let mut rng = StdRng::seed_from_u64(seed);
    (0..n)
        .map(|_| (0..dims).map(|_| rng.gen_range(-1.0f32..=1.0)).collect())
        .collect()
}

fn anisotropy_ratio(vecs: &[Vec<f32>]) -> f32 {
    let dims = vecs[0].len();
    let mut min_v = vec![f32::INFINITY; dims];
    let mut max_v = vec![f32::NEG_INFINITY; dims];
    for v in vecs {
        for (d, &x) in v.iter().enumerate() {
            if x < min_v[d] {
                min_v[d] = x;
            }
            if x > max_v[d] {
                max_v[d] = x;
            }
        }
    }
    let ranges: Vec<f32> = (0..dims).map(|d| max_v[d] - min_v[d]).collect();
    let max_r = ranges.iter().cloned().fold(0.0f32, f32::max);
    let min_r = ranges
        .iter()
        .cloned()
        .filter(|&r| r > 1e-12)
        .fold(f32::INFINITY, f32::min);
    if min_r.is_finite() && min_r > 0.0 {
        max_r / min_r
    } else {
        1.0
    }
}

/// Microbench: f32 dot vs GsSq8Codec L2 vs Sq8Codec dot at 384d, 1M evaluations.
///
/// Pool size 10K causes cache pressure (f32 ~15MB, u8 ~3.75MB), simulating ANN traversal.
///
/// Reports ns/call for:
///   f32_dot       — baseline LLVM-vectorized f32 dot product
///   sq8_dot       — Sq8Codec::approx_dot (NEON u8 pass + per-dim residual)
///   sq8_l2sq      — Sq8Codec::approx_l2_sq (NEON u8 pass + per-dim residual)
///   gs_l2_sq      — GsSq8Codec::l2_sq (pure NEON u8 integer path, ~13ns)
#[test]
#[ignore]
fn probe_sq8_distance_micro() {
    let dims = 384;
    let n_pairs: u64 = 1_000_000;
    let pool_size = 10_000;

    let vecs = gen_uniform_384d(pool_size, dims, 0x4D_4943_524F_BEAD);
    let ratio = anisotropy_ratio(&vecs);
    println!("  corpus: {pool_size}×{dims}d uniform [-1,1]  anisotropy_ratio={ratio:.2}");

    // Train per-dim codec (dot/cosine path)
    let sq8 = Sq8Codec::train(&vecs);
    let sq8_enc: Vec<_> = vecs.iter().map(|v| sq8.encode(v)).collect();

    // Train global-scale codec (L2 path)
    let gs = GsSq8Codec::train(&vecs);
    let gs_enc: Vec<_> = vecs.iter().map(|v| gs.encode(v)).collect();

    println!(
        "  GsSq8Codec: gs={:.6}  anisotropy_ratio={:.2}",
        gs.gs, gs.anisotropy_ratio
    );

    // --- f32 dot baseline ---
    let flat: Vec<f32> = vecs.iter().flatten().copied().collect();
    let mut f32_sink = 0.0f32;
    let t0 = Instant::now();
    for idx in 0..n_pairs as usize {
        let a_start = (idx % pool_size) * dims;
        let b_start = ((idx + 1) % pool_size) * dims;
        let a = &flat[a_start..a_start + dims];
        let b = &flat[b_start..b_start + dims];
        f32_sink += a.iter().zip(b.iter()).map(|(x, y)| x * y).sum::<f32>();
    }
    let f32_ns = t0.elapsed().as_nanos() as f64 / n_pairs as f64;

    // --- Sq8Codec approx_dot (NEON integer + f32 residual) ---
    let mut sq8_dot_sink = 0.0f32;
    let t0 = Instant::now();
    for idx in 0..n_pairs as usize {
        sq8_dot_sink += sq8.approx_dot(&sq8_enc[idx % pool_size], &sq8_enc[(idx + 1) % pool_size]);
    }
    let sq8_dot_ns = t0.elapsed().as_nanos() as f64 / n_pairs as f64;

    // --- Sq8Codec approx_l2_sq (NEON + f32 residual) ---
    let mut sq8_l2_sink = 0.0f32;
    let t0 = Instant::now();
    for idx in 0..n_pairs as usize {
        sq8_l2_sink += sq8.approx_l2_sq(&sq8_enc[idx % pool_size], &sq8_enc[(idx + 1) % pool_size]);
    }
    let sq8_l2_ns = t0.elapsed().as_nanos() as f64 / n_pairs as f64;

    // --- GsSq8Codec l2_sq (pure NEON integer, no residual) ---
    let mut gs_l2_sink = 0.0f32;
    let t0 = Instant::now();
    for idx in 0..n_pairs as usize {
        gs_l2_sink += gs.l2_sq(&gs_enc[idx % pool_size], &gs_enc[(idx + 1) % pool_size]);
    }
    let gs_l2_ns = t0.elapsed().as_nanos() as f64 / n_pairs as f64;

    let dot_speedup = f32_ns / sq8_dot_ns.max(1e-3);
    let l2_speedup = f32_ns / sq8_l2_ns.max(1e-3);
    let gs_speedup = f32_ns / gs_l2_ns.max(1e-3);

    println!("PROBE probe_sq8_distance_micro ({dims}d, {n_pairs}M evals, pool={pool_size}):");
    println!("  f32_dot:       {f32_ns:.1}ns/call  (sink={f32_sink:.1})");
    println!("  sq8_dot:       {sq8_dot_ns:.1}ns/call  speedup={dot_speedup:.2}x  [NEON+residual]");
    println!("  sq8_l2sq:      {sq8_l2_ns:.1}ns/call  speedup={l2_speedup:.2}x  [NEON+residual]");
    println!(
        "  gs_l2_sq:      {gs_l2_ns:.1}ns/call  speedup={gs_speedup:.2}x  [NEON only, no residual]"
    );

    // Prevent dead-code elimination of sink accumulators.
    if f32_sink + sq8_dot_sink + sq8_l2_sink + gs_l2_sink < -1e30 {
        panic!("unreachable sink guard");
    }
}
