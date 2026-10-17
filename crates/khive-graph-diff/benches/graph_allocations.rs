#[cfg(not(target_arch = "wasm32"))]
#[path = "../tests/support/allocation_counter.rs"]
mod allocation_counter;
#[cfg(not(target_arch = "wasm32"))]
#[allow(dead_code)]
#[path = "support/fixtures.rs"]
mod fixtures;
#[cfg(not(target_arch = "wasm32"))]
#[path = "support/selection.rs"]
mod selection;

#[cfg(not(target_arch = "wasm32"))]
#[global_allocator]
static ALLOCATOR: allocation_counter::ObservedSystem = allocation_counter::ObservedSystem;

#[cfg(not(target_arch = "wasm32"))]
fn report(name: &str, records_per_input: usize, measured: allocation_counter::Measurement) {
    println!(
        "{}",
        serde_json::json!({
            "scenario": name, "status": "MEASURED", "statistic": "single_call", "calls": 1,
            "records_per_input": records_per_input,
            "rust_global_allocator_requests_per_call": measured.successful_requests,
            "rust_global_allocator_requests_per_input_record": measured.successful_requests as f64 / records_per_input as f64,
            "peak_live_rust_heap_above_pre_call_baseline_bytes": measured.peak_incremental_rust_live_bytes
        })
    );
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    use allocation_counter::Observation;
    use fixtures::{DiffFixture, ParseFixture, Relation, Stream};
    use khive_graph_diff::diff;
    use std::hint::black_box;

    let selection = selection::Selection::from_args();
    let mut integrity_checked = false;
    let mut ensure_integrity = || {
        if !integrity_checked {
            allocation_counter::integrity_check();
            integrity_checked = true;
        }
    };
    for stream in Stream::ALL {
        for count in [1_000, 50_000, 500_000] {
            let name = format!("parse/{}/{count}", stream.name());
            if !selection.selected(&name, count) {
                continue;
            }
            ensure_integrity();
            let fixture = ParseFixture::new(stream, count);
            let observation = Observation::begin();
            let graph = black_box(&fixture).parse();
            let measured = observation.finish();
            fixture.validate(&graph);
            report(&name, count, measured);
        }
    }
    for relation in Relation::ALL {
        for count in [1_000, 50_000] {
            let name = format!("diff/{}/{count}", relation.name());
            if !selection.selected(&name, count) {
                continue;
            }
            ensure_integrity();
            let fixture = DiffFixture::new(relation, count);
            let observation = Observation::begin();
            let output = diff(black_box(&fixture.before), black_box(&fixture.after));
            let measured = observation.finish();
            fixture.validate(&output);
            report(&name, 3 * count, measured);
        }
    }
}

#[cfg(target_arch = "wasm32")]
fn main() {}
