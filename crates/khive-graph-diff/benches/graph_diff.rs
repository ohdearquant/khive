#[cfg(not(target_arch = "wasm32"))]
#[allow(dead_code)]
#[path = "support/fixtures.rs"]
mod fixtures;

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use super::fixtures::{DiffFixture, Relation};
    use criterion::{BatchSize, BenchmarkId, Criterion, SamplingMode, Throughput};
    use khive_graph_diff::diff;
    use std::hint::black_box;
    use std::time::Duration;

    pub fn run() {
        let mut c = Criterion::default()
            .sample_size(10)
            .warm_up_time(Duration::from_millis(100))
            .measurement_time(Duration::from_secs(1))
            .configure_from_args();
        let mut group = c.benchmark_group("graph_diff");
        group.sampling_mode(SamplingMode::Flat);
        for relation in Relation::ALL {
            for count in [1_000, 50_000] {
                // Lazily construct once for a selected scene, then drop before the next.
                let mut fixture = None;
                group.throughput(Throughput::Elements((3 * count) as u64));
                group.bench_function(BenchmarkId::new(relation.name(), count), |b| {
                    let fixture = fixture.get_or_insert_with(|| DiffFixture::new(relation, count));
                    b.iter_batched(
                        || (),
                        |_| black_box(diff(black_box(&fixture.before), black_box(&fixture.after))),
                        BatchSize::PerIteration,
                    );
                });
            }
        }
        group.finish();
        c.final_summary();
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    native::run();
}

#[cfg(target_arch = "wasm32")]
fn main() {}
