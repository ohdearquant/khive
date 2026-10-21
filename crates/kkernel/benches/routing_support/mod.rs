use std::time::Duration;

use criterion::measurement::WallTime;
use criterion::{BenchmarkGroup, Criterion};
use uuid::Uuid;

const REPORT_SAMPLES: usize = 1_001;

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

pub fn id(index: u64) -> Uuid {
    Uuid::from_u128((5_665_u128 << 64) | u128::from(index))
}

pub trait Workload {
    fn sample(&mut self) -> Duration;
}

pub fn group<'a>(c: &'a mut Criterion, name: &str) -> BenchmarkGroup<'a, WallTime> {
    let mut group = c.benchmark_group(name);
    group.sample_size(10);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(3));
    group
}

pub fn scene<W: Workload>(
    group: &mut BenchmarkGroup<'_, WallTime>,
    name: &str,
    mut make: impl FnMut() -> W,
) {
    let reports = reports_enabled();
    let mut workload = None;
    let mut reported = false;
    // Criterion invokes this closure only for selected, non-list scenarios.
    group.bench_function(name, |b| {
        let workload = workload.get_or_insert_with(&mut make);
        if reports && !reported {
            let _ = workload.sample();
            let mut durations = Vec::with_capacity(REPORT_SAMPLES);
            for _ in 0..REPORT_SAMPLES {
                durations.push(workload.sample());
            }
            durations.sort_unstable();
            let p99_rank = (99 * REPORT_SAMPLES).div_ceil(100);
            println!(
                "routing_call_report scenario={name} samples={REPORT_SAMPLES} median_ns={} p99_ns={}",
                durations[REPORT_SAMPLES / 2].as_nanos(),
                durations[p99_rank - 1].as_nanos()
            );
            reported = true;
        }
        b.iter_custom(|iterations| {
            let mut total = Duration::ZERO;
            for _ in 0..iterations {
                total += workload.sample();
            }
            total
        });
    });
}
