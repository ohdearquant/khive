//! Smoke tests for the schedule pack (ADR-040).

use khive_pack_schedule::SchedulePack;
use khive_runtime::{KhiveRuntime, VerbRegistry, VerbRegistryBuilder};
use khive_types::Pack;

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

async fn event_properties(runtime: &KhiveRuntime, full_id: &str) -> serde_json::Value {
    let token = runtime
        .authorize(khive_runtime::Namespace::local())
        .expect("local token");
    runtime
        .notes(&token)
        .expect("note store")
        .get_note(full_id.parse().expect("full UUID"))
        .await
        .expect("read event")
        .expect("event exists")
        .properties
        .expect("event properties")
}

include!("integration_tests/registration_and_creation_tests.rs");

include!("integration_tests/timestamp_validation_tests.rs");

include!("integration_tests/cancellation_and_past_date_tests.rs");

include!("integration_tests/actions_and_windows_tests.rs");

include!("integration_tests/offsets_and_legacy_rows_tests.rs");

include!("integration_tests/schema_cron_and_limit_tests.rs");

include!("integration_tests/corrupt_cancellation_tests.rs");
