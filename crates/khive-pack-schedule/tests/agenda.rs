//! Agenda query, filtering, pagination, and limit validation tests.

use khive_pack_schedule::SchedulePack;
use khive_runtime::{KhiveRuntime, VerbRegistry, VerbRegistryBuilder};

mod support;

fn build_registry() -> (VerbRegistry, KhiveRuntime) {
    let runtime = support::memory_runtime();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(khive_pack_comm::CommPack::new(runtime.clone()));
    builder.register(SchedulePack::new(runtime.clone()));
    let registry = builder.build().expect("registry builds");
    (registry, runtime)
}

include!("agenda_tests/visibility_tests.rs");

include!("agenda_tests/time_window_tests.rs");

include!("agenda_tests/legacy_pagination_tests.rs");

include!("agenda_tests/limit_and_timestamp_tests.rs");
