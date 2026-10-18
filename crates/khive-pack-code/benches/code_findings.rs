//! Pure record conversion, with generation and output checking/destruction untimed.
use std::time::{Duration, Instant};

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use khive_pack_code::{ingest_clippy_json_lines, ingest_findings_json, CodeIngestBatch};
use uuid::Uuid;

#[path = "support/findings.rs"]
mod findings;

fn sample(call: &impl Fn() -> CodeIngestBatch, expected: &[Uuid]) -> Duration {
    let start = Instant::now();
    let output = black_box(call());
    let elapsed = start.elapsed();
    assert_eq!(findings::identity(&output, expected.len()), expected);
    drop(output);
    elapsed
}

fn conversion(c: &mut Criterion) {
    let mut group = c.benchmark_group("pack_code_findings");
    group.sample_size(10);
    for count in findings::SIZES {
        let json = findings::findings(count);
        let jsonl = findings::clippy(count);
        let options = findings::options();
        let provenance = findings::provenance();
        for (name, call) in [
            (
                "json",
                Box::new(|| {
                    ingest_findings_json(black_box(&json), options.clone()).expect("findings map")
                }) as Box<dyn Fn() -> CodeIngestBatch + '_>,
            ),
            (
                "clippy_jsonl",
                Box::new(|| {
                    ingest_clippy_json_lines(black_box(&jsonl), provenance, options.clone())
                        .expect("Clippy map")
                }) as Box<dyn Fn() -> CodeIngestBatch + '_>,
            ),
        ] {
            let expected = findings::identity(&call(), count);
            let elapsed = sample(&call, &expected);
            println!(
                "{}",
                serde_json::json!({"scenario":format!("pack_code_findings/{name}"),"findings":count,
                "ns_per_finding":elapsed.as_secs_f64()*1e9/count as f64,"findings_per_second":count as f64/elapsed.as_secs_f64()})
            );
            group.throughput(Throughput::Elements(count as u64));
            group.bench_with_input(BenchmarkId::new(name, count), &call, |b, call| {
                b.iter_custom(|iterations| {
                    (0..iterations)
                        .map(|_| sample(call, &expected))
                        .sum::<Duration>()
                });
            });
        }
    }
    group.finish();
}

criterion_group!(benches, conversion);
criterion_main!(benches);
