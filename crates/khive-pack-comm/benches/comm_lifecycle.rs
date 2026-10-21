//! Isolated dispatch measurements; setup, validation and destruction are untimed.
mod lifecycle_support;

use std::time::{Duration, Instant};

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use khive_runtime::RuntimeError;
use serde_json::Value;

use lifecycle_support::{executor, Prepared, Scene};

const REPORT_SAMPLES: usize = 11;

fn sample(rt: &tokio::runtime::Runtime, scene: Scene) -> Duration {
    let mut prepared = rt.block_on(Prepared::new(scene));
    let (first, second) = prepared.take_calls();
    let start = Instant::now();
    let result: Result<(Value, Option<Value>), RuntimeError> = rt.block_on(async {
        let first = prepared
            .fixture
            .registry
            .dispatch(first.0, black_box(first.1))
            .await?;
        let second = match second {
            Some((verb, params)) => Some(
                prepared
                    .fixture
                    .registry
                    .dispatch(verb, black_box(params))
                    .await?,
            ),
            None => None,
        };
        Ok((first, second))
    });
    let elapsed = start.elapsed();
    let result = result.expect("successful complete dispatch");
    rt.block_on(prepared.check(&result));
    drop(result);
    rt.block_on(prepared.close());
    elapsed
}

fn lifecycle(c: &mut Criterion) {
    let rt = executor();
    let mut group = c.benchmark_group("comm_lifecycle");
    group.sample_size(10);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(3));
    group.throughput(Throughput::Elements(1));
    for scene in Scene::ALL {
        let mut reported = false;
        group.bench_function(scene.name(), |b| {
            if !reported {
                let mut observed: Vec<_> =
                    (0..REPORT_SAMPLES).map(|_| sample(&rt, scene)).collect();
                observed.sort_unstable();
                // Eleven actual operations: median is sorted rank6, not a batch mean.
                let median = observed[REPORT_SAMPLES / 2];
                println!(
                    "{}",
                    serde_json::json!({
                        "scenario": scene.name(), "samples":REPORT_SAMPLES,
                        "median_ns_per_operation":median.as_nanos(),
                        "median_ms_per_operation":median.as_secs_f64()*1000.0,
                        "operation":scene.operation(),
                    })
                );
                reported = true;
            }
            b.iter_custom(|iterations| {
                (0..iterations)
                    .map(|_| sample(&rt, scene))
                    .sum::<Duration>()
            });
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    lifecycle,
    lifecycle_support::thread_rendering::benchmark
);
criterion_main!(benches);
