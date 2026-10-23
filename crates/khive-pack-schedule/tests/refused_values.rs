//! Scheduled create validation masks refused kind values before echoing them.
use khive_pack_schedule::SchedulePack;
use khive_runtime::{
    runtime_error_value, DomainDisposition, RuntimeError, VerbRegistry, VerbRegistryBuilder,
};
use serde_json::json;

mod support;

fn fixture() -> VerbRegistry {
    let runtime = support::memory_runtime();
    assert!(runtime.default_embedder_name().is_empty());
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some("schedule-refusal-fixture".into()));
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(SchedulePack::new(runtime));
    builder.build().expect("registry")
}

fn action(field: &str, raw: &str, bulk: bool) -> String {
    let mut entry = match field {
        "kind" => json!({"kind":raw, "name":"refusal target"}),
        "entity_kind" => json!({"kind":"entity", "name":"refusal target"}),
        "note_kind" => json!({"kind":"note", "content":"refusal target"}),
        _ => panic!("unexpected test field"),
    };
    entry[field] = json!(raw);
    if bulk {
        format!("create(items=[{entry}])")
    } else {
        let args = entry
            .as_object()
            .unwrap()
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!("create({args})")
    }
}

fn expected(registry: &VerbRegistry, field: &str, shown: &str, bulk: bool) -> String {
    if field == "kind" {
        return format!(
            "schedule.action: verb \"create\" with kind={shown:?} always fails at replay (unknown \
             kind; valid: entity | note | edge | event | proposal | {} | {})",
            registry.all_entity_kinds().join(" | "),
            registry.all_note_kinds().join(" | ")
        );
    }
    let mut kinds = if field == "entity_kind" {
        registry.all_entity_kinds()
    } else {
        registry.all_note_kinds()
    };
    kinds.sort_unstable();
    let context = if bulk { "items[0] " } else { "" };
    format!(
        "schedule.action: verb \"create\": {context}invalid input: unknown {field} {shown:?}; valid: {}",
        kinds.join(" | ")
    )
}

async fn assert_kind_refusals(field: &str, valid: &str) {
    let registry = fixture();
    for bulk in [false, true] {
        for (raw, shown) in [
            ("bogus".to_string(), "bogus"),
            (format!("sk-proj-{}", "A".repeat(80)), "***MASKED***"),
        ] {
            let error = registry
                .dispatch(
                    "schedule.schedule",
                    json!({"action":action(field, &raw, bulk), "at":"2099-06-01T10:00:00Z"}),
                )
                .await
                .expect_err("refused kind");
            let RuntimeError::InvalidInput(message) = &error else {
                panic!("changed refusal variant");
            };
            if raw != "bogus" {
                assert!(!message.contains(&raw), "exposed the synthetic credential");
                assert!(message.contains("***MASKED***"));
            }
            let expected = expected(&registry, field, shown, bulk);
            assert_eq!(message, &expected);
            assert_eq!(
                runtime_error_value(error, DomainDisposition::NotCommitted),
                json!({"kind":"runtime_error", "message":format!("invalid input: {expected}"),
                    "domain_disposition":"not_committed"})
            );
        }
    }
    let agenda = registry
        .dispatch("schedule.agenda", json!({}))
        .await
        .unwrap();
    assert_eq!(agenda["count"], 0, "refusals must not schedule an event");

    let mut accepted_actions = Vec::new();
    for bulk in [false, true] {
        let action = action(field, valid, bulk);
        let response = registry
            .dispatch(
                "schedule.schedule",
                json!({"action":action, "at":"2099-06-01T10:00:00Z"}),
            )
            .await
            .expect("valid kind must still schedule");
        assert_eq!(response["status"], "pending");
        accepted_actions.push(action);
    }
    let agenda = registry
        .dispatch("schedule.agenda", json!({}))
        .await
        .unwrap();
    assert_eq!(agenda["count"], 2);
    let mut stored_actions: Vec<_> = agenda["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["properties"]["payload"].as_str().unwrap().to_string())
        .collect();
    stored_actions.sort();
    accepted_actions.sort();
    assert_eq!(
        stored_actions, accepted_actions,
        "stored actions retain their bytes"
    );
}

#[tokio::test]
async fn schedule_masks_refused_create_kinds_in_singleton_and_bulk_actions() {
    assert_kind_refusals("kind", "concept").await;
}

#[tokio::test]
async fn schedule_masks_refused_entity_kinds_in_singleton_and_bulk_actions() {
    assert_kind_refusals("entity_kind", "concept").await;
}

#[tokio::test]
async fn schedule_masks_refused_note_kinds_in_singleton_and_bulk_actions() {
    assert_kind_refusals("note_kind", "observation").await;
}
