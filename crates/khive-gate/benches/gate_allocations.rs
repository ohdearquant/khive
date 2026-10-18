//! Isolated count instrumentation; latency baselines belong to gate_lifecycle.
use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use khive_gate::CLASSIFIED_OPERATIONS;
use serde_json::json;

#[path = "../tests/support/allocation_counter.rs"]
mod allocation_counter;
#[path = "../tests/support/classification_probe.rs"]
mod classification_probe;

fn allocations(c: &mut Criterion) {
    assert!(!CLASSIFIED_OPERATIONS.is_empty());
    let (probe, observed) =
        allocation_counter::measure(|| black_box(Box::new(black_box([42_u8; 64]))));
    assert!(observed > 0, "observer must detect a real heap allocation");
    drop(probe);
    for &(verb, expected) in CLASSIFIED_OPERATIONS {
        let calls = classification_probe::FIXED_CALLS;
        let allocations = classification_probe::allocation_count(verb, expected, calls);
        println!(
            "{}",
            json!({"scenario":"classify_allocations","verb":verb,"table_size":CLASSIFIED_OPERATIONS.len(),"calls":calls,"allocations":allocations,"alloc_per_call":allocations as f64 / calls as f64})
        );
    }
    let mut group = c.benchmark_group("gate_allocations");
    group.sample_size(10);
    group.throughput(Throughput::Elements(CLASSIFIED_OPERATIONS.len() as u64));
    group.bench_function("all_verbs", |b| {
        b.iter(|| {
            for &(verb, expected) in CLASSIFIED_OPERATIONS {
                black_box(classification_probe::allocation_count(verb, expected, 1));
            }
        })
    });
    group.finish();
}

criterion_group!(benches, allocations);
criterion_main!(benches);
