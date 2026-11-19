mod lifecycle_support;

use criterion::{criterion_group, criterion_main};

criterion_group!(
    benches,
    lifecycle_support::mutations::bench_updates,
    lifecycle_support::mutations::bench_merge,
    lifecycle_support::mutations::bench_bulk,
    lifecycle_support::reads::bench_count,
    lifecycle_support::reads::bench_traverse
);
criterion_main!(benches);
