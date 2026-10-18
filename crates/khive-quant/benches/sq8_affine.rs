//! Per-dimension affine SQ8 distance costs at fixed dimensionalities.

mod support;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use khive_quant::Sq8Codec;
use std::{hint::black_box, time::Duration};

fn bench_affine(c: &mut Criterion) {
    let mut group = c.benchmark_group("sq8_affine/scaling");
    for dims in [64usize, 128, 256, 384, 768] {
        let corpus = support::corpus(dims);
        let codec = Sq8Codec::train_flat(&corpus, dims);
        let a = codec.encode(&corpus[17 * dims..18 * dims]);
        let b = codec.encode(&corpus[29 * dims..30 * dims]);
        assert_eq!(codec.dims(), dims);
        assert_eq!(a.codes.len(), dims);
        assert_eq!(b.codes.len(), dims);
        assert!(codec.approx_dot(&a, &b).is_finite());
        assert!((0.0..=2.0).contains(&codec.approx_cosine_dist(&a, &b)));
        assert!(codec.approx_l2_sq(&a, &b).is_finite());
        assert_eq!(codec.approx_l2_sq(&a, &a), 0.0);

        group.bench_with_input(BenchmarkId::new("approx_dot", dims), &dims, |bencher, _| {
            bencher.iter(|| black_box(black_box(&codec).approx_dot(black_box(&a), black_box(&b))));
        });
        group.bench_with_input(
            BenchmarkId::new("approx_cosine_dist", dims),
            &dims,
            |bencher, _| {
                bencher.iter(|| {
                    black_box(black_box(&codec).approx_cosine_dist(black_box(&a), black_box(&b)))
                });
            },
        );
        group.bench_with_input(
            BenchmarkId::new("approx_l2_sq", dims),
            &dims,
            |bencher, _| {
                bencher.iter(|| {
                    black_box(black_box(&codec).approx_l2_sq(black_box(&a), black_box(&b)))
                });
            },
        );
    }
    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .sample_size(30)
        .warm_up_time(Duration::from_millis(100))
        .measurement_time(Duration::from_secs(1));
    targets = bench_affine
}
criterion_main!(benches);
