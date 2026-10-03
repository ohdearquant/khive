use khive_runtime::{KhiveRuntime, Namespace, RuntimeError, VerbRegistryBuilder};
use serde_json::{json, Value};

fn invalid_input_reason(error: RuntimeError) -> String {
    match error {
        RuntimeError::InvalidInput(reason) => reason,
        other => panic!("expected a create validation refusal, got {other}"),
    }
}

#[tokio::test]
async fn singleton_scheduled_event_refusal_matches_create_dispatch() {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some("fixture-schedule-owner".into()));
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(crate::SchedulePack::new(runtime.clone()));
    let registry = builder.build().expect("real KG and schedule registry");
    let token = runtime.authorize(Namespace::local()).expect("local token");

    let ordinary = registry
        .dispatch(
            "schedule.schedule",
            json!({
                "action": "create(kind=\"note\", content=\"ordinary fixture\")",
                "at": "2099-06-01T09:00:00Z"
            }),
        )
        .await
        .expect("ordinary singleton note actions remain schedulable");
    assert_eq!(ordinary["status"], "pending");
    let baseline = runtime
        .list_notes(&token, Some("scheduled_event"), 20, 0)
        .await
        .expect("scheduled rows before refusals");
    assert_eq!(
        baseline.len(),
        1,
        "positive schedule guard must write one row"
    );

    for action in [
        "create(kind=\"scheduled_event\", content=\"fixture\")",
        "create(kind=\"note\", note_kind=\"scheduled_event\", content=\"fixture\")",
        "create(kind=\" SCHEDULED_EVENT \", content=\"fixture\")",
        "create(kind=\"note\", note_kind=\" SCHEDULED_EVENT \", content=\"fixture\")",
    ] {
        let parsed = khive_request::parse_request(action).expect("literal singleton action");
        assert_eq!(parsed.ops.len(), 1);
        let op = &parsed.ops[0];
        assert_eq!(op.tool, "create");
        // Feed exactly the action's arguments to the real create handler.
        let params = Value::Object(
            op.args
                .iter()
                .map(|(name, arg)| {
                    (
                        name.clone(),
                        arg.as_value().expect("literal argument").clone(),
                    )
                })
                .collect(),
        );
        let create_reason = invalid_input_reason(
            registry
                .dispatch("create", params)
                .await
                .expect_err("KG create must refuse scheduled_event"),
        );
        assert!(
            create_reason.contains("`created_by_actor` is a trust boundary"),
            "the create dispatch must reach its scheduled-event ownership refusal: {create_reason}"
        );
        let schedule_reason = invalid_input_reason(
            registry
                .dispatch(
                    "schedule.schedule",
                    json!({"action": action, "at": "2099-06-01T09:00:00Z"}),
                )
                .await
                .expect_err("schedule-time validation must refuse the same scheduled_event action"),
        );
        assert_eq!(schedule_reason, create_reason, "refusal drift for {action}");
        let after = runtime
            .list_notes(&token, Some("scheduled_event"), 20, 0)
            .await
            .expect("scheduled rows after refusal");
        assert_eq!(
            after.len(),
            baseline.len(),
            "refused action must not be stored"
        );
        assert_eq!(
            after[0].id, baseline[0].id,
            "existing schedule must survive"
        );
    }
}
