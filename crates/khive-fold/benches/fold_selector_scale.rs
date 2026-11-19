#[path = "workload_support/selector.rs"]
mod selector;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, SamplingMode};
use khive_fold::{GreedySelector, Selector};
use std::hint::black_box;
use std::time::{Duration, Instant};

fn benches(c: &mut Criterion) {
    for (name, sizes, precision) in [
        ("selector/epistemic_gain", [100, 500], true),
        ("selector/diversity_scale", [1_000, 5_000], false),
    ] {
        let mut group = c.benchmark_group(name);
        group.sampling_mode(SamplingMode::Flat);
        for count in sizes {
            let mut fixture = None;
            group.bench_function(BenchmarkId::from_parameter(count), |b| {
                let fixture = fixture.get_or_insert_with(|| {
                    if precision {
                        selector::Fixture::precision(count)
                    } else {
                        selector::Fixture::diversity(count)
                    }
                });
                b.iter_custom(|iters| {
                    let mut total = Duration::ZERO;
                    for _ in 0..iters {
                        let input = fixture.inputs.clone();
                        let start = Instant::now();
                        let output = GreedySelector.select(
                            black_box(input),
                            black_box(fixture.budget),
                            black_box(&fixture.weights),
                        );
                        total += start.elapsed();
                        fixture.validate(&output.expect("complete bounded selection"));
                    }
                    total
                });
            });
        }
        group.finish();
    }
}
criterion_group! { name = selection; config = Criterion::default().sample_size(10).warm_up_time(Duration::from_millis(100)).measurement_time(Duration::from_secs(1)); targets = benches }
criterion_main!(selection);
