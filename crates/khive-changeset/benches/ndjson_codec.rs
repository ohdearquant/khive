#[cfg(not(target_arch = "wasm32"))]
#[path = "support/fixtures.rs"]
mod fixtures;

#[cfg(not(target_arch = "wasm32"))]
fn ndjson_codec(c: &mut criterion::Criterion) {
    use criterion::{black_box, BenchmarkId, Throughput};
    use fixtures::{Fixture, Shape};
    use khive_changeset::{from_ndjson, to_ndjson};
    let mut group = c.benchmark_group("ndjson_round_trip");
    group.sample_size(10);
    group.warm_up_time(std::time::Duration::from_secs(1));
    group.measurement_time(std::time::Duration::from_secs(3));
    for count in [1, 100, 10_000] {
        group.throughput(Throughput::Elements(count as u64));
        for (name, shape) in [
            ("entity_create", Shape::Create),
            ("entity_update", Shape::Update),
        ] {
            let mut fixture = None;
            group.bench_function(BenchmarkId::new(name, count), |b| {
                // Construction/preflight/reporting runs only for selected non-list scenes.
                let fixture = fixture.get_or_insert_with(|| Fixture::new(count, shape));
                b.iter(|| {
                    let encoded = to_ndjson(black_box(&fixture.forward)).expect("encode");
                    from_ndjson(black_box(&encoded)).expect("decode")
                });
            });
        }
    }
    group.finish();
}

#[cfg(not(target_arch = "wasm32"))]
criterion::criterion_group!(benches, ndjson_codec);
#[cfg(not(target_arch = "wasm32"))]
criterion::criterion_main!(benches);

// Portable library/tests stay independent of the native Criterion dependency.
#[cfg(target_arch = "wasm32")]
fn main() {}
