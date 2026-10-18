#[path = "email_support/routes.rs"]
mod routes;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use routes::{MarkingFixture, RouteFixture, EMAILS, MARKINGS, ROUTES};
use std::time::{Duration, Instant};

fn benches(c: &mut Criterion) {
    for (group_name, scenes, marking) in [
        ("route_inbound", ROUTES, false),
        ("external_delivery_marking", MARKINGS, true),
    ] {
        let mut group = c.benchmark_group(group_name);
        group.throughput(Throughput::Elements(EMAILS as u64));
        for scene in scenes {
            let mut reported = false;
            group.bench_function(scene, |b| {
                if marking {
                    let fixture = MarkingFixture::new(scene);
                    let mut output = Vec::with_capacity(EMAILS);
                    if !reported { reported = true;
                    let start = Instant::now(); fixture.run(&mut output); let elapsed = start.elapsed();
                    fixture.validate(&output);
                    eprintln!("{group_name}/{scene}: emails={EMAILS}, fixed_batch_ns={}, ns_per_email={:.2}", elapsed.as_nanos(), elapsed.as_nanos() as f64 / EMAILS as f64); }
                    b.iter_custom(|iters| {
                        let mut total = Duration::ZERO;
                        for _ in 0..iters {
                            output.clear();
                            let start = Instant::now(); fixture.run(&mut output); total += start.elapsed();
                            fixture.validate(&output);
                        }
                        total
                    });
                } else {
                    let fixture = RouteFixture::new(scene);
                    let mut output = Vec::with_capacity(EMAILS);
                    if !reported { reported = true;
                    let start = Instant::now(); fixture.run(&mut output); let elapsed = start.elapsed();
                    fixture.validate(&output);
                    eprintln!("{group_name}/{scene}: emails={EMAILS}, fixed_batch_ns={}, ns_per_email={:.2}", elapsed.as_nanos(), elapsed.as_nanos() as f64 / EMAILS as f64); }
                    b.iter_custom(|iters| {
                        let mut total = Duration::ZERO;
                        for _ in 0..iters {
                            output.clear();
                            let start = Instant::now(); fixture.run(&mut output); total += start.elapsed();
                            fixture.validate(&output);
                        }
                        total
                    });
                }
            });
        }
        group.finish();
    }
}

criterion_group! { name = email; config = Criterion::default().sample_size(10).warm_up_time(Duration::from_secs(1)).measurement_time(Duration::from_secs(3)); targets = benches }
criterion_main!(email);
