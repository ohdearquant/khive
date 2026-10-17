//! Instrumented allocation accounting; use channel_lifecycle for timing baselines.
use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use serde_json::json;

#[path = "../tests/support/allocation_counter.rs"]
mod allocation_counter;
#[path = "support/signed_fixture.rs"]
mod signed_fixture;

fn count_verifies(fixture: &signed_fixture::SignatureFixture, count: usize) -> usize {
    let (all_valid, allocations) = allocation_counter::measure(|| {
        let mut all_valid = true;
        for _ in 0..count {
            all_valid &= fixture
                .key
                .verify(black_box(&fixture.input), black_box(&fixture.signature))
                .is_ok();
        }
        black_box(all_valid)
    });
    assert!(all_valid, "real signature checks must succeed");
    assert_eq!(allocations, 0, "prepared signature verification allocated");
    allocations
}

fn allocations(c: &mut Criterion) {
    let (probe, observed) =
        allocation_counter::measure(|| black_box(Box::new(black_box([42_u8; 64]))));
    assert!(
        observed > 0,
        "allocation observer must detect a real heap request"
    );
    drop(probe);
    let fixture = signed_fixture::SignatureFixture::new();
    for count in [1, 1000] {
        let observed = count_verifies(&fixture, count);
        println!(
            "{}",
            json!({"scenario":"sig_verify_allocations","verifies":count,"allocations":observed,"alloc_per_verify":observed as f64 / count as f64})
        );
    }
    let mut group = c.benchmark_group("channel_allocations");
    group.sample_size(10);
    group.throughput(Throughput::Elements(1000));
    group.bench_function("sig_verify_1000", |b| {
        b.iter(|| black_box(count_verifies(&fixture, 1000)))
    });
    group.finish();
}

criterion_group!(benches, allocations);
criterion_main!(benches);
