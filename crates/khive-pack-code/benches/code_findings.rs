//! Pure record conversion, with generation and output checking/destruction untimed.
use std::cell::OnceCell;
use std::time::{Duration, Instant};

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use khive_pack_code::{ingest_clippy_json_lines, ingest_findings_json, CodeIngestBatch};
use uuid::Uuid;

#[path = "support/findings.rs"]
mod findings;
#[path = "support/modes.rs"]
mod modes;

fn sample(call: &impl Fn() -> CodeIngestBatch, expected: &[Uuid]) -> Duration {
    let start = Instant::now();
    let output = black_box(call());
    let elapsed = start.elapsed();
    assert_eq!(findings::identity(&output, expected.len()), expected);
    drop(output);
    elapsed
}

fn conversion(c: &mut Criterion) {
    let reports = modes::reports_enabled();
    let mut group = c.benchmark_group("pack_code_findings");
    group.sample_size(10);
    for count in findings::SIZES {
        for name in ["json", "clippy_jsonl"] {
            let input = OnceCell::new();
            let expected = OnceCell::new();
            let mut reported = false;
            group.throughput(Throughput::Elements(count as u64));
            group.bench_with_input(BenchmarkId::new(name, count), &count, |b, &count| {
                let input = input.get_or_init(|| {
                    if name == "json" {
                        findings::findings(count)
                    } else {
                        findings::clippy(count)
                    }
                });
                let options = findings::options();
                let provenance = findings::provenance();
                let call: Box<dyn Fn() -> CodeIngestBatch + '_> = if name == "json" {
                    Box::new(|| {
                        ingest_findings_json(black_box(input), options.clone())
                            .expect("findings map")
                    })
                } else {
                    Box::new(|| {
                        ingest_clippy_json_lines(black_box(input), provenance, options.clone())
                            .expect("Clippy map")
                    })
                };
                let expected = expected.get_or_init(|| findings::identity(&call(), count));
                if reports && !reported {
                    let elapsed = sample(&call, expected);
                    println!(
                        "{}",
                        serde_json::json!({"scenario":format!("pack_code_findings/{name}"),"findings":count,
                        "ns_per_finding":elapsed.as_secs_f64()*1e9/count as f64,"findings_per_second":count as f64/elapsed.as_secs_f64()})
                    );
                    reported = true;
                }
                b.iter_custom(|iterations| {
                    (0..iterations)
                        .map(|_| sample(&call, expected))
                        .sum::<Duration>()
                });
            });
        }
    }
    group.finish();
}

criterion_group!(benches, conversion);
criterion_main!(benches);
