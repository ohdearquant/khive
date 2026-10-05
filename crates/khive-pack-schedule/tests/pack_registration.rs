//! Pack registration and schema plan tests.

use khive_pack_schedule::SchedulePack;
use khive_runtime::VerbRegistryBuilder;
use khive_types::Pack;

mod support;

#[test]
fn schedule_pack_declares_scheduled_event_note_kind() {
    assert!(SchedulePack::NOTE_KINDS.contains(&"scheduled_event"));
}

#[test]
fn schedule_pack_declares_four_handlers() {
    assert_eq!(SchedulePack::HANDLERS.len(), 4);
    let names: Vec<&str> = SchedulePack::HANDLERS.iter().map(|h| h.name).collect();
    assert!(names.contains(&"schedule.remind"));
    assert!(names.contains(&"schedule.schedule"));
    assert!(names.contains(&"schedule.agenda"));
    assert!(names.contains(&"schedule.cancel"));
}

#[test]
fn schedule_pack_requires_kg() {
    assert_eq!(SchedulePack::REQUIRES, &["kg"]);
}

#[test]
fn schedule_pack_builds_registry_without_comm() {
    let runtime = support::memory_runtime();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(SchedulePack::new(runtime));
    builder.build().expect("kg + schedule registry builds");
}

/// Issue #897: `schedule.remind` gates on `registry.describe_verb("comm.send")`
/// at dispatch time. This must be exercised against the real multi-pack
/// registry-build path (kg + comm + schedule registered together through
/// `VerbRegistryBuilder`, the same construction production pack loading
/// uses), not a mock or a hand-rolled stand-in for the comm pack. A prior
/// incident conflated two independent server processes in the same smoke
/// run and mistook this for a broken capability-discovery mechanism; this
/// test proves the mechanism itself is correct when both packs are actually
/// registered together.
#[tokio::test]
async fn schedule_pack_registry_with_comm_dispatches_remind() {
    let runtime = support::memory_runtime();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(khive_pack_comm::CommPack::new(runtime.clone()));
    builder.register(SchedulePack::new(runtime));
    let registry = builder
        .build()
        .expect("kg + comm + schedule registry builds");

    registry.describe_verb("comm.send").expect(
        "comm.send must be discoverable once the comm pack is registered alongside schedule",
    );

    let result = registry
        .dispatch(
            "schedule.remind",
            serde_json::json!({
                "content": "pack-registration regression: comm capability visible",
                "at": "2099-06-01T09:00:00Z",
            }),
        )
        .await
        .expect("schedule.remind must succeed when comm is registered alongside schedule");

    assert!(result.get("id").is_some(), "remind returns id: {result}");
    assert_eq!(result["status"], "pending");
}

#[tokio::test]
async fn schedule_pack_has_no_auxiliary_schema() {
    use khive_runtime::PackRuntime;
    let pack = SchedulePack::new(support::memory_runtime());
    assert!(
        pack.schema_plan().is_empty(),
        "schedule indexes on core tables belong to migration V51"
    );
    assert!(<SchedulePack as Pack>::SCHEMA_PLAN.is_none());
}

#[tokio::test]
async fn registry_includes_schedule_without_core_table_ddl() {
    let runtime = support::memory_runtime();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(SchedulePack::new(runtime));
    let registry = builder.build().expect("registry builds");
    let mut names = registry.pack_names();
    names.sort_unstable();
    assert_eq!(names, ["kg", "schedule"]);
    let plans = registry.all_schema_plans();
    assert_eq!(plans.len(), 2, "both registered packs contribute a plan");
    assert!(
        plans.iter().all(|plan| plan.is_empty()),
        "kg and schedule need no auxiliary DDL; core indexes come from migrations"
    );
}
