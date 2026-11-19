#[path = "knowledge_support/fixture.rs"]
mod fixture;
#[path = "knowledge_support/workloads.rs"]
mod workloads;

use criterion::{criterion_group, criterion_main, Criterion};
use std::time::Duration;

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(10)
        .warm_up_time(Duration::from_millis(100)).measurement_time(Duration::from_secs(1));
    targets = workloads::compose_suggest, workloads::fold, workloads::search_scale,
        workloads::search_matrix
}
criterion_main!(benches);
