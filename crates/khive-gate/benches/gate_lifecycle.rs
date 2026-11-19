//! Uninstrumented classifier distributions and actual mailbox policy composition.
use std::time::{Duration, Instant};

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use khive_gate::{
    check_with_mailbox_policy, classify_operation, mailbox_read_owner, CLASSIFIED_OPERATIONS,
};
use serde_json::json;

#[path = "support/mailbox_fixture.rs"]
mod mailbox_fixture;

const CLASSIFY_SAMPLES: usize = 1_000_000;
const MAILBOX_CALLS: usize = 100_000;
const MAILBOX_CHUNK: usize = 128;

fn classify_distributions() {
    assert!(!CLASSIFIED_OPERATIONS.is_empty());
    assert!(CLASSIFIED_OPERATIONS
        .windows(2)
        .all(|pair| pair[0].0 < pair[1].0));
    // Reuse one 8 MB logical sample payload, never one vector per verb.
    let mut samples = Vec::<u64>::with_capacity(CLASSIFY_SAMPLES);
    for &(verb, expected) in CLASSIFIED_OPERATIONS {
        samples.clear();
        for _ in 0..CLASSIFY_SAMPLES {
            let start = Instant::now();
            let result = black_box(classify_operation(black_box(verb)));
            let elapsed = start.elapsed();
            assert_eq!(result, Some(expected), "classifier result for {verb}");
            samples.push(u64::try_from(elapsed.as_nanos()).expect("sample fits u64 nanoseconds"));
        }
        samples.sort_unstable();
        let middle = samples.len() / 2;
        let median = (samples[middle - 1] as f64 + samples[middle] as f64) / 2.0;
        let p99_index = (99 * samples.len()).div_ceil(100) - 1;
        println!(
            "{}",
            json!({"scenario":"classify_per_call","verb":verb,"table_size":CLASSIFIED_OPERATIONS.len(),"samples":samples.len(),"median_ns":median,"p99_ns":samples[p99_index],"measurement":"individual calls with clock/harness overhead; nearest-rank p99"})
        );
    }
}

fn classify_batch() -> Duration {
    let mut all_match = true;
    let start = Instant::now();
    for &(verb, expected) in CLASSIFIED_OPERATIONS {
        all_match &= black_box(classify_operation(black_box(verb))) == Some(expected);
    }
    black_box(all_match);
    let elapsed = start.elapsed();
    assert!(all_match, "every exported classification must match");
    elapsed
}

fn mailbox_batch<T>(calls: usize, operation: impl Fn() -> T, check: impl Fn(T)) -> Duration {
    let mut elapsed = Duration::ZERO;
    let mut results = Vec::with_capacity(MAILBOX_CHUNK);
    let mut checked = 0;
    for offset in (0..calls).step_by(MAILBOX_CHUNK) {
        let chunk = MAILBOX_CHUNK.min(calls - offset);
        let start = Instant::now();
        for _ in 0..chunk {
            results.push(black_box(operation()));
        }
        elapsed += start.elapsed();
        for result in results.drain(..) {
            check(result);
            checked += 1;
        }
    }
    assert_eq!(checked, calls);
    elapsed
}

fn report_mailbox(function: &str, case: &str, elapsed: Duration) {
    println!(
        "{}",
        json!({"scenario":"mailbox_fixed","function":function,"case":case,"calls":MAILBOX_CALLS,"elapsed_ns":elapsed.as_nanos(),"ns_per_call":elapsed.as_nanos() as f64 / MAILBOX_CALLS as f64})
    );
}

fn lifecycle(c: &mut Criterion) {
    classify_distributions();
    let mut group = c.benchmark_group("gate_classify");
    group.sample_size(10);
    group.throughput(Throughput::Elements(CLASSIFIED_OPERATIONS.len() as u64));
    group.bench_function("all_verbs", |b| {
        b.iter_custom(|iterations| (0..iterations).map(|_| classify_batch()).sum::<Duration>())
    });
    group.finish();

    let (gate, cases) = mailbox_fixture::fixture();
    let mut group = c.benchmark_group("gate_mailbox_policy");
    group.sample_size(10);
    group.throughput(Throughput::Elements(MAILBOX_CHUNK as u64));
    for case in &cases {
        report_mailbox(
            "check_with_mailbox_policy",
            case.name,
            mailbox_batch(
                MAILBOX_CALLS,
                || check_with_mailbox_policy(black_box(&gate), black_box(&case.request)),
                |result| case.assert_decision(result),
            ),
        );
        group.bench_function(case.name, |b| {
            b.iter_custom(|iterations| {
                (0..iterations)
                    .map(|_| {
                        mailbox_batch(
                            MAILBOX_CHUNK,
                            || {
                                check_with_mailbox_policy(
                                    black_box(&gate),
                                    black_box(&case.request),
                                )
                            },
                            |result| case.assert_decision(result),
                        )
                    })
                    .sum::<Duration>()
            })
        });
    }
    group.finish();

    let mut group = c.benchmark_group("gate_mailbox_owner");
    group.sample_size(10);
    group.throughput(Throughput::Elements(MAILBOX_CHUNK as u64));
    for case in &cases {
        report_mailbox(
            "mailbox_read_owner",
            case.name,
            mailbox_batch(
                MAILBOX_CALLS,
                || mailbox_read_owner(black_box(&case.request)),
                |result| case.assert_owner(result),
            ),
        );
        group.bench_function(case.name, |b| {
            b.iter_custom(|iterations| {
                (0..iterations)
                    .map(|_| {
                        mailbox_batch(
                            MAILBOX_CHUNK,
                            || mailbox_read_owner(black_box(&case.request)),
                            |result| case.assert_owner(result),
                        )
                    })
                    .sum::<Duration>()
            })
        });
    }
    group.finish();
}

criterion_group!(benches, lifecycle);
criterion_main!(benches);
