#[cfg(not(target_arch = "wasm32"))]
#[path = "support/fixtures.rs"]
mod fixtures;

#[cfg(not(target_arch = "wasm32"))]
fn invert(c: &mut criterion::Criterion) {
    use criterion::{black_box, BatchSize, BenchmarkId, Throughput};
    use fixtures::{Fixture, Shape};
    let mut group = c.benchmark_group("changeset_invert");
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
                let fixture = fixture.get_or_insert_with(|| Fixture::new(count, shape));
                // Exclude caller envelope cloning and returned output destruction.
                // The public API's internal CurrentState clone remains timed.
                b.iter_batched(
                    || fixture.inverse_envelope.clone(),
                    |envelope| {
                        black_box(&fixture.forward)
                            .invert(black_box(envelope), black_box(&fixture.current))
                            .expect("inverse")
                    },
                    BatchSize::PerIteration,
                );
            });
        }
    }
    group.finish();
}

#[cfg(not(target_arch = "wasm32"))]
criterion::criterion_group!(benches, invert);
#[cfg(not(target_arch = "wasm32"))]
criterion::criterion_main!(benches);

#[cfg(target_arch = "wasm32")]
fn main() {}
