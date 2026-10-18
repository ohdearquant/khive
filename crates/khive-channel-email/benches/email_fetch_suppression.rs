#[path = "email_support/http.rs"]
mod http;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use std::time::Duration;

fn benches(c: &mut Criterion) {
    let mut group = c.benchmark_group("fetch_remote_suppression_set");
    for count in [1_000, 10_000] {
        let mut reported = false;
        group.bench_function(BenchmarkId::from_parameter(count), |b| {
            let fixture = http::Fixture::new(count); let rt = http::executor();
            if !reported { reported = true;
            let prepared = fixture.prepare(&rt); let sample = fixture.run(&rt, prepared); fixture.validate(&sample);
            eprintln!("fetch_remote_suppression_set/{count}: entries={count}, fixed_call_ns={} (loopback HTTP client plus parser)", sample.elapsed.as_nanos());
            }
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters { let prepared = fixture.prepare(&rt); let sample = fixture.run(&rt, prepared); total += sample.elapsed; fixture.validate(&sample); }
                total
            });
        });
    }
    group.finish();
}
criterion_group! { name = email; config = Criterion::default().sample_size(10).warm_up_time(Duration::from_secs(1)).measurement_time(Duration::from_secs(3)); targets = benches }
criterion_main!(email);
