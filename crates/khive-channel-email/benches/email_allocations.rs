//! Fixed allocation reports only; this binary does not produce latency baselines.
#[path = "../tests/support/allocation_counter.rs"]
mod allocation_counter;
#[path = "email_support/backoff.rs"]
mod backoff;
#[path = "email_support/http.rs"]
mod http;
#[path = "email_support/routes.rs"]
mod routes;

use allocation_counter::{Measurement, Observation};
use routes::{MarkingFixture, RouteFixture, EMAILS, MARKINGS, ROUTES};

#[global_allocator]
static ALLOCATOR: allocation_counter::ObservedSystem = allocation_counter::ObservedSystem;

fn report(name: &str, operations: usize, measured: Measurement) {
    println!(
        "{}",
        serde_json::json!({"scenario":name,"operations":operations,
        "rust_global_allocator_requests":measured.successful_requests,
        "rust_global_allocator_requests_per_operation":measured.successful_requests as f64 / operations as f64,
        "peak_live_rust_heap_above_pre_call_baseline_bytes":measured.peak_incremental_rust_live_bytes})
    );
}

fn main() {
    // A fixed report accepts Cargo's bench flags, with literal substring selection.
    let mut filter = None;
    let mut list = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--bench" | "--quick" | "--noplot" | "--test" => {}
            "--list" => list = true,
            "--help" | "-h" => {
                println!("email_allocations [SUBSTRING] [--list] [--quick] [--noplot]: fixed allocation reports, no timing samples");
                return;
            }
            _ if !arg.starts_with('-') && filter.is_none() => filter = Some(arg),
            _ => panic!("unsupported allocation-report argument; see --help"),
        }
    }
    let mut integrity_checked = false;
    let mut selected = |name: &str| {
        if filter.as_ref().is_some_and(|filter| !name.contains(filter)) {
            return false;
        }
        if list {
            println!("{name}: benchmark");
            return false;
        }
        if !integrity_checked {
            allocation_counter::integrity_check();
            integrity_checked = true;
        }
        true
    };
    for scene in ROUTES {
        let name = format!("allocations/route_inbound/{scene}");
        if !selected(&name) {
            continue;
        }
        let fixture = RouteFixture::new(scene);
        let mut output = Vec::with_capacity(EMAILS);
        fixture.run(&mut output);
        fixture.validate(&output);
        output.clear();
        let observation = Observation::begin();
        fixture.run(&mut output);
        let measured = observation.finish();
        fixture.validate(&output);
        report(&name, EMAILS, measured);
    }
    for scene in MARKINGS {
        let name = format!("allocations/external_delivery_marking/{scene}");
        if !selected(&name) {
            continue;
        }
        let fixture = MarkingFixture::new(scene);
        let mut output = Vec::with_capacity(EMAILS);
        fixture.run(&mut output);
        fixture.validate(&output);
        output.clear();
        let observation = Observation::begin();
        fixture.run(&mut output);
        let measured = observation.finish();
        fixture.validate(&output);
        report(&name, EMAILS, measured);
    }
    for (scene, prior) in [("first", 0), ("escalating", 3), ("saturated", 10)] {
        let name = format!("allocations/backoff_record_failure/{scene}");
        if !selected(&name) {
            continue;
        }
        let fixture = backoff::BackoffFixture::new(prior);
        let mut state = fixture.state.clone();
        let observation = Observation::begin();
        let result = state.record_failure();
        let measured = observation.finish();
        fixture.validate(result);
        report(&name, 1, measured);
    }
    let name = "allocations/single_flight/uncontended";
    if selected(name) {
        let rt = backoff::executor();
        let guard = khive_channel_email::ImapSingleFlight::new();
        drop(rt.block_on(guard.acquire()));
        let observation = Observation::begin();
        let permit = rt.block_on(guard.acquire());
        let measured = observation.finish();
        drop(permit);
        report(name, 1, measured);
    }
    for count in [1, 100] {
        let name = format!("allocations/single_flight/queued/{count}");
        if !selected(&name) {
            continue;
        }
        let rt = backoff::executor();
        let mut queue = backoff::Queue::new(&rt, count);
        let observation = Observation::begin();
        queue.enqueue();
        let _elapsed = queue.release_and_collect(&rt);
        let measured = observation.finish();
        queue.validate(&rt);
        report(&name, count, measured);
    }
    for count in [1_000, 10_000] {
        let name = format!("allocations/fetch_remote_suppression_set/{count}");
        if !selected(&name) {
            continue;
        }
        let fixture = http::Fixture::new(count);
        let rt = http::executor();
        let warm = fixture.run(&rt, fixture.prepare(&rt));
        fixture.validate(&warm);
        drop(warm);
        let prepared = fixture.prepare(&rt);
        let observation = Observation::begin();
        let sample = fixture.run(&rt, prepared);
        let measured = observation.finish();
        fixture.validate(&sample);
        std::hint::black_box(sample.elapsed);
        report(&name, 1, measured);
    }
}
