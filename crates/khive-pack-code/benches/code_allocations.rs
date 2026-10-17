//! Fixed allocation reports; this instrumented binary never provides latency baselines.
use criterion::{black_box, Criterion};
use khive_pack_code::source_ingest::run_code_ingest;
use khive_pack_code::{ingest_clippy_json_lines, ingest_findings_json, CodeIngestBatch};
use khive_runtime::Namespace;

#[path = "../tests/support/allocation_counter.rs"]
mod allocation_counter;
#[path = "support/findings.rs"]
mod findings;
#[path = "support/source.rs"]
mod source;

#[global_allocator]
static ALLOCATOR: allocation_counter::ObservedSystem = allocation_counter::ObservedSystem;

fn main() {
    // Accept the normal Criterion runner arguments. Fixed reports always execute
    // every listed size, including quick/test mode; they are not Criterion times.
    let _arguments = Criterion::default().configure_from_args();
    allocation_counter::integrity_check();
    let rt = source::executor();
    for files in source::SIZES {
        let tree = source::SourceTree::new(files);
        let fixture = rt.block_on(async { source::MapFixture::fresh() });
        assert_eq!(rt.block_on(fixture.counts()), [0, 0, 0]);
        let token = fixture
            .runtime
            .authorize(Namespace::local())
            .expect("local token");
        let options = tree.options();
        let observation = allocation_counter::Observation::begin();
        let result = rt.block_on(run_code_ingest(
            &fixture.runtime,
            &token,
            black_box(options),
        ));
        let measured = observation.finish();
        let counts = rt.block_on(fixture.counts());
        drop(token);
        rt.block_on(fixture.close());
        tree.check(&result.expect("complete ingest"));
        assert_eq!(counts, [1, files as i64, files as i64]);
        println!(
            "{}",
            serde_json::json!({"scenario":format!("pack_code_allocations/fresh_l1_l15/{files}"),"source_files":files,
            "rust_global_allocator_calls":measured.successful_requests,
            "rust_global_allocator_calls_per_source_file":measured.successful_requests as f64 / files as f64,
            "peak_live_rust_heap_above_pre_call_baseline_bytes":measured.peak_incremental_rust_live_bytes})
        );
    }
    drop(rt);
    for count in findings::SIZES {
        let input = findings::findings(count);
        let jsonl = findings::clippy(count);
        let options = findings::options();
        let provenance = findings::provenance();
        for (name, call) in [
            (
                "json",
                Box::new(|| {
                    ingest_findings_json(black_box(&input), options.clone()).expect("findings map")
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
            let observation = allocation_counter::Observation::begin();
            let result = black_box(call());
            let measured = observation.finish();
            assert_eq!(findings::identity(&result, count), expected);
            drop(result);
            println!(
                "{}",
                serde_json::json!({"scenario":format!("pack_code_allocations/{name}/{count}"),"findings":count,
                "rust_global_allocator_calls":measured.successful_requests,
                "peak_live_rust_heap_above_pre_call_baseline_bytes":measured.peak_incremental_rust_live_bytes})
            );
        }
    }
}
