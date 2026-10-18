#[path = "scale_support/agenda.rs"]
mod agenda;
#[path = "scale_support/recurrence.rs"]
mod recurrence;

use criterion::{criterion_group, criterion_main, Criterion, SamplingMode, Throughput};
use std::hint::black_box;
use std::time::{Duration, Instant};

fn benches(c: &mut Criterion) {
    let mut group = c.benchmark_group("recurrence");
    group.sampling_mode(SamplingMode::Flat);
    group.throughput(Throughput::Elements(recurrence::STEPS as u64));
    for first_after in [false, true] {
        let name = if first_after {
            "cron_first_after/1000"
        } else {
            "cron_next_after/1000"
        };
        let mut fixture = None;
        let mut reported = false;
        group.bench_function(name, |b| {
            let fixture = fixture.get_or_insert_with(recurrence::Fixture::new);
            let mut output = vec![fixture.base; recurrence::STEPS];
            if !reported {
                let elapsed = fixture.sample(first_after, &mut output);
                println!("{}", serde_json::json!({
                    "scenario": format!("recurrence/{name}"), "statistic":"single_batch_average",
                    "steps":recurrence::STEPS, "batch_ns":elapsed.as_nanos(),
                    "ns_per_step":elapsed.as_nanos() as f64 / recurrence::STEPS as f64,
                    "boundary":"public advancement plus timestamp-slot writes; parse, reset and validation excluded"
                }));
                reported = true;
            }
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    total += fixture.sample(first_after, &mut output);
                }
                total
            });
        });
    }
    group.finish();

    let mut group = c.benchmark_group("schedule_scale");
    group.sampling_mode(SamplingMode::Flat);
    let mut fixture = None;
    group.bench_function("agenda/1000_limit20", |b| {
        let fixture = fixture.get_or_insert_with(agenda::Fixture::new);
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let params = serde_json::json!({"limit":20});
                let start = Instant::now();
                let response = fixture.rt.block_on(
                    fixture
                        .registry
                        .dispatch("schedule.agenda", black_box(params)),
                );
                total += start.elapsed();
                fixture.validate_page(&response.expect("agenda succeeds"), 0, 20);
            }
            total
        });
    });
    group.finish();
    if let Some(fixture) = fixture {
        fixture.close();
    }
}

criterion_group! {
    name = workloads;
    config = Criterion::default().sample_size(10)
        .warm_up_time(Duration::from_millis(100)).measurement_time(Duration::from_secs(1));
    targets = benches
}
criterion_main!(workloads);
