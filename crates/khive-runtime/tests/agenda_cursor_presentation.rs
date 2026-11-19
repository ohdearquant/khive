//! Agenda's trusted root continuation survives the machine presentation seam.

use khive_runtime::presentation::{
    prepare_format_value, present_with_policy_at, OutputFormat, PresentationMode,
};
use khive_types::{HandlerDef, VerbCategory, VerbPresentationPolicy, Visibility};
use serde_json::{json, Value};

const ID: &str = "aabbccdd-1234-4321-1234-abcdefabcdef";
const AT: &str = "2099-01-01T12:00:00.123456+02:00";

fn policy(name: &'static str) -> VerbPresentationPolicy {
    HandlerDef {
        name,
        description: "test handler",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[],
    }
    .presentation_policy()
}

fn machine(value: Value, name: &'static str) -> Value {
    prepare_format_value(
        present_with_policy_at(value, PresentationMode::Agent, 0.into(), policy(name)),
        OutputFormat::Json,
        PresentationMode::Agent,
    )
}

#[test]
fn agenda_root_cursor_survives_agent_json_without_protecting_rows_or_lookalikes() {
    let cursor = json!({"after": AT, "after_id": ID});
    let value = json!({
        "events": [{"id": ID, "full_id": ID, "created_at": AT,
                    "properties": {"trigger_at": AT}}],
        "count": 1, "next": cursor,
        "nested": {"next": {"after": AT, "after_id": ID}},
    });
    let shown = machine(value.clone(), "schedule.agenda");
    assert_eq!(shown["next"], cursor);
    assert_eq!(shown["events"][0]["id"], "aabbccdd");
    assert_eq!(shown["events"][0]["full_id"], ID);
    assert_eq!(shown["events"][0]["properties"]["trigger_at"], AT);
    assert_eq!(
        shown["events"][0]["created_at"],
        "2099-01-01T10:00:00.123456Z"
    );
    assert_eq!(shown["nested"]["next"]["after_id"], "aabbccdd");
    assert_eq!(
        shown["nested"]["next"]["after"],
        "2099-01-01T10:00:00.123456Z"
    );
    let unrelated = machine(value, "schedule.remind");
    assert_eq!(unrelated["next"]["after_id"], "aabbccdd");
    assert_eq!(unrelated["next"]["after"], "2099-01-01T10:00:00.123456Z");
}

#[test]
fn agenda_empty_completion_signals_survive_only_the_registered_policy() {
    let empty = json!({"events": [], "count": 0, "next": null});
    assert_eq!(machine(empty.clone(), "schedule.agenda"), empty);
    assert_eq!(
        machine(empty.clone(), "schedule.remind"),
        json!({"count": 0})
    );
    for mode in [PresentationMode::Verbose, PresentationMode::Human] {
        assert_eq!(
            present_with_policy_at(empty.clone(), mode, 0.into(), policy("schedule.agenda")),
            empty
        );
    }
}
