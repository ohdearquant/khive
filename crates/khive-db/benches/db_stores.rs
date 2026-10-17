//! Bounded, generated storage workloads. See docs/benchmarks.md for timing boundaries.

use criterion::{criterion_group, criterion_main};

#[path = "store_workloads/fixture.rs"]
mod fixture;
#[path = "store_workloads/reads.rs"]
mod reads;
#[path = "store_workloads/writes.rs"]
mod writes;

criterion_group!(
    store_benches,
    writes::bench_writes,
    reads::bench_entities,
    reads::bench_graph,
    reads::bench_notes,
    reads::bench_events
);
criterion_main!(store_benches);
