#[path = "email_support/backoff.rs"]
mod backoff;

use backoff::{executor, BackoffFixture, Queue};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use khive_channel_email::ImapSingleFlight;
use std::hint::black_box;
use std::time::{Duration, Instant};

fn benches(c: &mut Criterion) {
    let mut group = c.benchmark_group("backoff_record_failure");
    for (name, prior) in [("first", 0), ("escalating", 3), ("saturated", 10)] {
        group.bench_function(name, |b| {
            let fixture = BackoffFixture::new(prior);
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    let mut state = fixture.state.clone();
                    let start = Instant::now();
                    let tick = black_box(&mut state).record_failure();
                    total += start.elapsed();
                    fixture.validate(tick);
                }
                total
            });
        });
    }
    group.finish();
    let mut group = c.benchmark_group("single_flight");
    group.bench_function("uncontended", |b| {
        let rt = executor();
        let guard = ImapSingleFlight::new();
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let (elapsed, permit) = rt.block_on(async {
                    let start = Instant::now();
                    let permit = guard.acquire().await;
                    (start.elapsed(), permit)
                });
                total += elapsed;
                drop(permit);
            }
            total
        });
    });
    for count in [1, 100] {
        group.throughput(Throughput::Elements(count as u64));
        let mut reported = false;
        group.bench_function(BenchmarkId::new("queued", count), |b| {
            let rt = executor();
            if !reported {
                reported = true;
                let mut fixed = Queue::new(&rt, count);
                fixed.enqueue();
                let elapsed = fixed.release_and_collect(&rt);
                fixed.validate(&rt);
                eprintln!(
                    "single_flight/queued/{count}: batch_ns={}, ns_per_acquire={:.2}",
                    elapsed.as_nanos(),
                    elapsed.as_nanos() as f64 / count as f64
                );
                report_waits(&format!("single_flight/queued/{count}"), &mut fixed.waits);
            }
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    let mut queue = Queue::new(&rt, count);
                    queue.enqueue();
                    total += queue.release_and_collect(&rt);
                    queue.validate(&rt);
                }
                total
            });
        });
    }
    group.finish();
}
criterion_group! { name = email; config = Criterion::default().sample_size(10).warm_up_time(Duration::from_secs(1)).measurement_time(Duration::from_secs(3)); targets = benches }
criterion_main!(email);

fn report_waits(label: &str, waits: &mut [Duration]) {
    assert!(!waits.is_empty());
    waits.sort_unstable();
    let rank = |percent: usize| (waits.len() * percent).div_ceil(100) - 1;
    eprintln!("{label}: actual_acquires={}, median_ns={}, p99_ns={} (nearest rank, queued release-to-acquire)", waits.len(), waits[rank(50)].as_nanos(), waits[rank(99)].as_nanos());
}
