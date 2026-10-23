pub mod fixtures;

use std::time::{Duration, Instant};

use criterion::measurement::WallTime;
use criterion::{black_box, BenchmarkGroup};

const REPORT_SAMPLES: usize = 1001;

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

fn sample<P, R>(
    prepare: &mut impl FnMut() -> P,
    operation: &mut impl FnMut(P) -> R,
    check: &impl Fn(&R),
) -> Duration {
    let input = prepare();
    let start = Instant::now();
    let result = black_box(operation(black_box(input)));
    let elapsed = start.elapsed();
    check(&result);
    black_box(&result);
    elapsed
}

pub fn scene<P, R>(
    group: &mut BenchmarkGroup<'_, WallTime>,
    name: &str,
    mut prepare: impl FnMut() -> P,
    mut operation: impl FnMut(P) -> R,
    check: impl Fn(&R),
) {
    let reports = reports_enabled();
    let mut reported = false;
    group.bench_function(name, |b| {
        if reports && !reported {
            sample(&mut prepare, &mut operation, &check);
            let mut durations = Vec::with_capacity(REPORT_SAMPLES);
            for _ in 0..REPORT_SAMPLES {
                durations.push(sample(&mut prepare, &mut operation, &check));
            }
            durations.sort_unstable();
            println!(
                "request_call_report scenario={name} samples={REPORT_SAMPLES} median_ns={}",
                durations[REPORT_SAMPLES / 2].as_nanos()
            );
            reported = true;
        }
        b.iter_custom(|iterations| {
            let mut total = Duration::ZERO;
            for _ in 0..iterations {
                total += sample(&mut prepare, &mut operation, &check);
            }
            total
        });
    });
}
