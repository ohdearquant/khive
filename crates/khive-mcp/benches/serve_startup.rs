//! Real-process startup latency; fixture creation and process reaping are untimed.

mod startup_support;

use std::time::Duration;

use criterion::{Criterion, Throughput};
use startup_support::{executor, sample, HELPER_MARKER};

const REPORT_SAMPLES: usize = 101;

fn main() -> anyhow::Result<()> {
    // The helper owns only its MCP stdio; Criterion never shares that stream.
    if std::env::var_os(HELPER_MARKER).is_some() {
        return startup_support::helper();
    }
    let mut criterion = Criterion::default().configure_from_args();
    let runtime = executor();
    let mut latencies = Vec::with_capacity(REPORT_SAMPLES);
    for _ in 0..REPORT_SAMPLES {
        latencies.push(runtime.block_on(sample())?);
    }
    latencies.sort_unstable();
    // n=101: median rank51; nearest-rank p99=ceil(.99*101)=100.
    println!(
        "serve_startup: samples={REPORT_SAMPLES} entities=100 median_ns={} p99_ns={}",
        latencies[50].as_nanos(),
        latencies[99].as_nanos()
    );
    let mut group = criterion.benchmark_group("mcp");
    group.sample_size(10);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(3));
    group.throughput(Throughput::Elements(1));
    group.bench_function("serve_startup", |b| {
        b.iter_custom(|iterations| {
            let mut measured = Duration::ZERO;
            for _ in 0..iterations {
                // No simultaneous fixture batch or process: each call reaps first.
                measured += runtime.block_on(sample()).expect("stdio startup sample");
            }
            measured
        });
    });
    group.finish();
    criterion.final_summary();
    Ok(())
}
