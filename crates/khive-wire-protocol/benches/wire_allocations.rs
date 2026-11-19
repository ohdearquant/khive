//! Instrumented fixed reports; latency is measured only by the other binaries.
#[cfg(not(target_arch = "wasm32"))]
#[path = "../tests/support/allocation_counter.rs"]
mod allocation_counter;
#[cfg(not(target_arch = "wasm32"))]
#[path = "support/codec.rs"]
mod codec;
#[cfg(not(target_arch = "wasm32"))]
#[path = "support/handshake.rs"]
mod handshake;
#[cfg(not(target_arch = "wasm32"))]
#[path = "support/selection.rs"]
mod selection;
#[cfg(not(target_arch = "wasm32"))]
#[global_allocator]
static ALLOCATOR: allocation_counter::ObservedSystem = allocation_counter::ObservedSystem;

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    let selection = selection::Selection::from_args();
    let mut checked = false;
    for operation in codec::Operation::ALL {
        for size in codec::SIZES {
            let name = format!("wire_allocations/codec/{}/{}", operation.name(), size);
            if !selection.selected(&name) {
                continue;
            }
            if !checked {
                allocation_counter::integrity_check();
                checked = true;
            }
            let fixture = codec::Fixture::new(size);
            let observation = allocation_counter::Observation::begin();
            let output = operation.run(&fixture);
            let measured = observation.finish();
            fixture.validate(output);
            println!(
                "{}",
                serde_json::json!({"scenario":name, "operations":1,
                "json_payload_bytes":fixture.payload_bytes, "wire_bytes":fixture.wire.len(),
                "successful_rust_global_allocator_requests_per_operation":measured.successful_requests,
                "peak_live_rust_heap_above_pre_call_baseline_bytes":measured.peak_incremental_rust_live_bytes})
            );
        }
    }
    for operation in handshake::Operation::ALL {
        let name = format!("wire_allocations/handshake/{}", operation.name());
        if !selection.selected(&name) {
            continue;
        }
        if !checked {
            allocation_counter::integrity_check();
            checked = true;
        }
        let fixture = handshake::Fixture::new();
        let gate = fixture.fresh_gate();
        let observation = allocation_counter::Observation::begin();
        let output = operation.run(&fixture, gate);
        let measured = observation.finish();
        fixture.validate(output);
        println!(
            "{}",
            serde_json::json!({"scenario":name, "connections":1,
            "successful_rust_global_allocator_requests_per_connection":measured.successful_requests,
            "peak_live_rust_heap_above_pre_call_baseline_bytes":measured.peak_incremental_rust_live_bytes})
        );
    }
}
#[cfg(target_arch = "wasm32")]
fn main() {}
