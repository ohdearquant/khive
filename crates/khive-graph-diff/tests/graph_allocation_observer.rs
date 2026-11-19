#![cfg(not(target_arch = "wasm32"))]
#[path = "support/allocation_counter.rs"]
mod allocation_counter;
#[global_allocator]
static ALLOCATOR: allocation_counter::ObservedSystem = allocation_counter::ObservedSystem;
#[test]
fn observer_counts_worker_allocations_and_peak() {
    allocation_counter::integrity_check();
}
