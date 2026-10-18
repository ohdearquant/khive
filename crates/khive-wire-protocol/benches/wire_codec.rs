#[cfg(not(target_arch = "wasm32"))]
#[path = "support/codec.rs"]
mod codec;

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    use criterion::{BenchmarkId, Criterion, SamplingMode, Throughput};
    use std::time::{Duration, Instant};
    let mut c = Criterion::default()
        .sample_size(10)
        .warm_up_time(Duration::from_millis(100))
        .measurement_time(Duration::from_secs(1))
        .configure_from_args();
    let mut group = c.benchmark_group("wire_codec");
    group.sampling_mode(SamplingMode::Flat);
    for operation in codec::Operation::ALL {
        for size in codec::SIZES {
            let mut fixture = None;
            let mut reported = false;
            group.throughput(Throughput::Bytes(size as u64));
            group.bench_function(BenchmarkId::new(operation.name(), size), |b| {
                let fixture = fixture.get_or_insert_with(|| codec::Fixture::new(size));
                if !reported {
                    let start = Instant::now();
                    let output = operation.run(fixture);
                    let elapsed = start.elapsed();
                    fixture.validate(output);
                    let mb_per_second = if elapsed.is_zero() { None } else {
                        Some(fixture.payload_bytes as f64 / elapsed.as_secs_f64() / 1_000_000.0)
                    };
                    println!("{}", serde_json::json!({
                        "scenario":format!("wire_codec/{}/{}",operation.name(),size),
                        "statistic":"single_call", "operations":1, "json_payload_bytes":fixture.payload_bytes,
                        "wire_bytes":fixture.wire.len(), "ns_per_operation":elapsed.as_nanos(),
                        "payload_decimal_MB_per_second":mb_per_second,
                        "throughput_basis":"one JSON payload per operation, including round trip; no transport"
                    }));
                    reported = true;
                }
                b.iter_custom(|iters| {
                    let mut total = Duration::ZERO;
                    for _ in 0..iters {
                        let start = Instant::now();
                        let output = operation.run(fixture);
                        total += start.elapsed();
                        fixture.validate(output);
                    }
                    total
                });
            });
        }
    }
    group.finish();
    c.final_summary();
}

#[cfg(target_arch = "wasm32")]
fn main() {}
