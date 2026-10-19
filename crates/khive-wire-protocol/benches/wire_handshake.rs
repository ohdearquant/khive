#[cfg(not(target_arch = "wasm32"))]
#[path = "support/handshake.rs"]
mod handshake;

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    use criterion::{Criterion, SamplingMode, Throughput};
    use std::time::{Duration, Instant};
    let mut c = Criterion::default()
        .sample_size(10)
        .warm_up_time(Duration::from_millis(100))
        .measurement_time(Duration::from_secs(1))
        .configure_from_args();
    let mut group = c.benchmark_group("wire_handshake");
    group.sampling_mode(SamplingMode::Flat);
    group.throughput(Throughput::Elements(1));
    for operation in handshake::Operation::ALL {
        let mut fixture = None;
        let mut reported = false;
        group.bench_function(operation.name(), |b| {
            let fixture = fixture.get_or_insert_with(handshake::Fixture::new);
            if !reported {
                let gate = fixture.fresh_gate();
                let start = Instant::now();
                let output = operation.run(fixture, gate);
                let elapsed = start.elapsed();
                fixture.validate(output);
                println!("{}", serde_json::json!({"scenario":format!("wire_handshake/{}", operation.name()),
                    "statistic":"single_call", "connections":1, "ns_per_connection":elapsed.as_nanos(),
                    "scope":"pure protocol exchange plus first request admission; no I/O or dispatch"}));
                reported = true;
            }
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    let gate = fixture.fresh_gate();
                    let start = Instant::now();
                    let output = operation.run(fixture, gate);
                    total += start.elapsed();
                    fixture.validate(output);
                }
                total
            });
        });
    }
    group.finish();
    c.final_summary();
}
#[cfg(target_arch = "wasm32")]
fn main() {}
