//! Tests for remind/schedule creation and input validation (repeat scheduling included).

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

fn build_registry_with_brain() -> (VerbRegistry, KhiveRuntime) {
    let runtime = support::memory_runtime();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(khive_pack_comm::CommPack::new(runtime.clone()));
    builder.register(khive_pack_brain::BrainPack::new(runtime.clone()));
    builder.register(SchedulePack::new(runtime.clone()));
    let registry = builder.build().expect("registry builds");
    (registry, runtime)
}

/// A registry with a real type-declaring pack (git, which declares `adr` as
/// a `Document` subtype via its `ENTITY_TYPES`) loaded alongside schedule —
/// used to pin that `schedule.schedule` resolves `entity_type` against the
/// SAME boot-time composed registry as the live KG `create` handler, not
/// just the builtin table (PR #925).
fn build_registry_with_git() -> (VerbRegistry, KhiveRuntime) {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(khive_pack_git::GitPack::new(runtime.clone()));
    builder.register(SchedulePack::new(runtime.clone()));
    let registry = builder.build().expect("registry builds");
    (registry, runtime)
}

include!("create_validation_tests/creation_and_actor_tests.rs");

include!("create_validation_tests/timestamp_validation_tests.rs");

include!("create_validation_tests/action_and_timestamp_preservation_tests.rs");

include!("create_validation_tests/recurrence_contract_tests.rs");

include!("create_validation_tests/action_replayability_tests.rs");

include!("create_validation_tests/typed_singleton_replay_tests.rs");

include!("create_validation_tests/typed_bulk_replay_tests.rs");

include!("create_validation_tests/kind_alias_and_limit_tests.rs");

include!("create_validation_tests/bulk_note_replay_tests.rs");

/// Field names declared by `khive-pack-kg`'s `BulkCreateEntry`, read from its
/// source (the struct is crate-private, and this crate must not depend on the
/// KG pack outside tests).
fn kg_bulk_entry_field_names() -> Vec<String> {
    let source = include_str!("../../khive-pack-kg/src/handlers/params.rs");
    let body = source
        .split("pub(crate) struct BulkCreateEntry {")
        .nth(1)
        .expect("BulkCreateEntry is declared in the KG pack's params module")
        .split("\n}")
        .next()
        .expect("BulkCreateEntry has a closing brace");
    body.lines()
        .filter_map(|line| line.trim().strip_prefix("pub(crate) "))
        .filter_map(|declaration| declaration.split(':').next())
        .map(str::to_string)
        .collect()
}

include!("create_validation_tests/bulk_field_parity_tests.rs");
