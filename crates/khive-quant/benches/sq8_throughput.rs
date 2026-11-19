//! Training and serial encoding throughput on a generated 1000 x 384 corpus.

mod support;

use criterion::{criterion_group, criterion_main, BatchSize, Criterion, Throughput};
use khive_quant::{GsSq8Codec, Sq8Codec};
use std::{hint::black_box, time::Duration};

const DIMS: usize = 384;

fn bench_training(c: &mut Criterion) {
    let corpus = support::corpus(DIMS);
    let mut group = c.benchmark_group("sq8_throughput/train_flat_1000x384");
    group.throughput(Throughput::Elements(support::ROWS as u64));
    group.bench_function("Sq8Codec", |bencher| {
        bencher.iter_batched(
            || (),
            |()| Sq8Codec::train_flat(black_box(&corpus), black_box(DIMS)),
            BatchSize::SmallInput,
        );
    });
    group.bench_function("GsSq8Codec", |bencher| {
        bencher.iter_batched(
            || (),
            |()| GsSq8Codec::train_flat(black_box(&corpus), black_box(DIMS)),
            BatchSize::SmallInput,
        );
    });
    group.finish();
}

fn bench_encoding(c: &mut Criterion) {
    let corpus = support::corpus(DIMS);
    let affine = Sq8Codec::train_flat(&corpus, DIMS);
    let global = GsSq8Codec::train_flat(&corpus, DIMS);
    assert_eq!(affine.dims(), DIMS);
    assert_eq!(global.min.len(), DIMS);
    assert_eq!(corpus.chunks_exact(DIMS).len(), support::ROWS);
    for row in corpus.chunks_exact(DIMS) {
        assert_eq!(affine.encode(row).codes.len(), DIMS);
        assert_eq!(global.encode(row).codes.len(), DIMS);
    }

    let mut group = c.benchmark_group("sq8_throughput/encode_1000x384");
    group.throughput(Throughput::Elements(support::ROWS as u64));
    group.bench_function("Sq8Codec", |bencher| {
        bencher.iter_batched(
            || (),
            |()| {
                black_box(&corpus)
                    .chunks_exact(DIMS)
                    .map(|row| black_box(&affine).encode(black_box(row)))
                    .collect::<Vec<_>>()
            },
            BatchSize::LargeInput,
        );
    });
    group.bench_function("GsSq8Codec", |bencher| {
        bencher.iter_batched(
            || (),
            |()| {
                black_box(&corpus)
                    .chunks_exact(DIMS)
                    .map(|row| black_box(&global).encode(black_box(row)))
                    .collect::<Vec<_>>()
            },
            BatchSize::LargeInput,
        );
    });
    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .sample_size(30)
        .warm_up_time(Duration::from_millis(100))
        .measurement_time(Duration::from_secs(1));
    targets = bench_training, bench_encoding
}
criterion_main!(benches);
