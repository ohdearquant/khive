//! Fresh-fixture task dispatch timings, with untimed setup and verification.
mod maintenance_support;

use std::cell::OnceCell;
use std::time::{Duration, Instant};

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use maintenance_support::{Prepared, Scene};

fn reports_enabled() -> bool {
    let mut bench = false;
    for arg in std::env::args_os().skip(1).take_while(|arg| arg != "--") {
        if arg == "--test"
            || arg == "--profile-time"
            || arg
                .to_str()
                .is_some_and(|arg| arg.starts_with("--profile-time="))
        {
            return false;
        }
        bench |= arg == "--bench";
    }
    bench
}

fn sample(rt: &tokio::runtime::Runtime, scene: Scene) -> Duration {
    let mut prepared = rt.block_on(Prepared::new(scene));
    let start = Instant::now();
    let result = rt.block_on(prepared.run());
    let elapsed = start.elapsed();
    let result = result.expect("successful complete dispatch");
    rt.block_on(prepared.check(&result));
    drop(result);
    rt.block_on(prepared.close());
    elapsed
}

fn maintenance(c: &mut Criterion) {
    let rt = OnceCell::new();
    let reports = reports_enabled();
    let mut group = c.benchmark_group("gtd_maintenance");
    group.sample_size(10);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(3));
    group.throughput(Throughput::Elements(1));
    for scene in Scene::ALL {
        let mut reported = false;
        group.bench_function(scene.name(), |b| {
            let rt = rt.get_or_init(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .max_blocking_threads(2)
                    .build()
                    .expect("bounded executor")
            });
            if reports && !reported {
                let mut observed: Vec<_> = (0..11).map(|_| sample(rt, scene)).collect();
                observed.sort_unstable();
                let median = observed[5]; // Rank6 of11 actual complete operations.
                println!(
                    "{}",
                    serde_json::json!({
                        "scenario":scene.name(), "samples":11,
                        "median_ns_per_operation":median.as_nanos(),
                        "median_ms_per_operation":median.as_secs_f64()*1000.0,
                    })
                );
                reported = true;
            }
            b.iter_custom(|iterations| {
                (0..iterations).map(|_| sample(rt, scene)).sum::<Duration>()
            });
        });
    }
    group.finish();
}

criterion_group!(benches, maintenance);
criterion_main!(benches);
