//! Public time and continuation refusals redact detected credentials without writes.
use khive_pack_schedule::SchedulePack;
use khive_runtime::{
    runtime_error_value, DomainDisposition, KhiveRuntime, Namespace, RuntimeError, VerbRegistry,
    VerbRegistryBuilder,
};
use serde_json::json;
use uuid::Uuid;

mod support;

fn fixture() -> (KhiveRuntime, VerbRegistry) {
    let runtime = support::memory_runtime();
    assert!(runtime.registered_embedding_model_names().is_empty());
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some("schedule-time-refusals".into()));
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(khive_pack_comm::CommPack::new(runtime.clone()));
    builder.register(SchedulePack::new(runtime.clone()));
    (runtime, builder.build().expect("registry"))
}

fn operands() -> [(String, &'static str); 2] {
    [
        ("bogus".into(), "bogus"),
        (format!("sk-proj-{}", "A".repeat(80)), "***MASKED***"),
    ]
}

fn assert_refusal(error: RuntimeError, raw: &str, expected: String) {
    let RuntimeError::InvalidInput(message) = &error else {
        panic!("wrong refusal kind: {error:?}");
    };
    if raw != "bogus" {
        assert!(
            !message.contains(raw),
            "credential escaped from schedule input: {message}"
        );
    }
    assert_eq!(message, &expected);
    assert_eq!(
        runtime_error_value(error, DomainDisposition::NotCommitted),
        json!({"kind":"runtime_error", "message":format!("invalid input: {expected}"),
            "domain_disposition":"not_committed"})
    );
}

#[tokio::test]
async fn creation_at_refusals_mask_both_verbs_and_preserve_timestamp_bytes() {
    let (runtime, registry) = fixture();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let notes = runtime.notes(&token).unwrap();
    for verb in ["schedule.remind", "schedule.schedule"] {
        let mut args = if verb == "schedule.remind" {
            json!({"content":"time refusal fixture"})
        } else {
            json!({"action":"stats()"})
        };
        for (raw, shown) in operands() {
            args["at"] = json!(raw);
            let error = registry.dispatch(verb, args.clone()).await.unwrap_err();
            assert_refusal(
                error,
                &raw,
                format!(
                    "{}.at: must be an RFC 3339 timestamp (e.g. \"2027-01-01T00:00:00Z\"), got {shown:?}",
                    verb.strip_prefix("schedule.").unwrap()
                ),
            );
            assert_eq!(notes.count_notes("local", None).await.unwrap(), 0);
        }
    }
    for verb in ["schedule.remind", "schedule.schedule"] {
        let mut args = if verb == "schedule.remind" {
            json!({"content":"accepted offset"})
        } else {
            json!({"action":"stats()"})
        };
        args["at"] = json!(" 2099-06-01T10:00:00+05:30 ");
        let result = registry.dispatch(verb, args).await.unwrap();
        assert_eq!(result["trigger_at"], "2099-06-01T10:00:00+05:30");
        assert_eq!(result["status"], "pending");
        let id = result["full_id"].as_str().unwrap().parse().unwrap();
        let stored = notes.get_note(id).await.unwrap().unwrap();
        assert_eq!(
            stored.properties.unwrap()["trigger_at"],
            result["trigger_at"]
        );
    }
    assert_eq!(notes.count_notes("local", None).await.unwrap(), 2);
}

async fn assert_agenda_refusal(field: &str) {
    let (runtime, registry) = fixture();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let notes = runtime.notes(&token).unwrap();
    let mut ids = Vec::new();
    let mut snapshots = Vec::new();
    for at in [
        "2099-06-01T12:00:00+02:00",
        "2099-06-01T11:00:00Z",
        "2099-06-01T12:00:00Z",
    ] {
        let result = registry
            .dispatch(
                "schedule.remind",
                json!({"content":"agenda fixture","at":at}),
            )
            .await
            .unwrap();
        let id: Uuid = result["full_id"].as_str().unwrap().parse().unwrap();
        snapshots.push((
            id,
            serde_json::to_value(notes.get_note(id).await.unwrap().unwrap()).unwrap(),
        ));
        ids.push(result["full_id"].clone());
    }
    for (raw, shown) in operands() {
        let mut args = match field {
            "from" => json!({"to":"also-invalid"}),
            "to" => json!({"from":"2099-06-01T10:00:00Z"}),
            "after" => json!({"after_id":"also-invalid"}),
            "after_id" => json!({"after":"2099-06-01T10:00:00Z"}),
            _ => panic!("unexpected fixture field"),
        };
        args[field] = json!(raw);
        let expected = match field {
            "from" | "to" => format!(
                "agenda.{field}: must be an RFC 3339 timestamp (e.g. \"2027-01-01T00:00:00Z\"), got {shown:?}"
            ),
            "after" => format!("agenda.after: must be an RFC 3339 timestamp; got {shown:?}"),
            "after_id" => format!("agenda.after_id: must be a UUID; got {shown:?}"),
            _ => unreachable!(),
        };
        let error = registry
            .dispatch("schedule.agenda", args.clone())
            .await
            .unwrap_err();
        assert_refusal(error, &raw, expected);
        args["limit"] = json!(0);
        assert_eq!(
            registry
                .dispatch("schedule.agenda", args)
                .await
                .unwrap_err()
                .to_string(),
            "invalid input: agenda: `limit` must be between 1 and 200 (inclusive); got 0",
            "limit refusal must still precede timestamp validation"
        );
    }
    let mut args =
        json!({"from":"2099-06-01T10:00:00Z", "to":"2099-06-01T14:00:00+02:00", "limit":1});
    let first = registry
        .dispatch("schedule.agenda", args.clone())
        .await
        .unwrap();
    assert_eq!(first["count"], 1);
    assert_eq!(first["events"][0]["full_id"], ids[0]);
    assert_eq!(first["next"]["after"], "2099-06-01T12:00:00+02:00");
    args["after"] = first["next"]["after"].clone();
    args["after_id"] = first["next"]["after_id"].clone();
    args["limit"] = json!(2);
    let rest = registry.dispatch("schedule.agenda", args).await.unwrap();
    assert_eq!(rest["count"], 2);
    assert_eq!(rest["events"][0]["full_id"], ids[1]);
    assert_eq!(rest["events"][1]["full_id"], ids[2]);
    assert_eq!(notes.count_notes("local", None).await.unwrap(), 3);
    for (id, before) in snapshots {
        assert_eq!(
            serde_json::to_value(notes.get_note(id).await.unwrap().unwrap()).unwrap(),
            before,
            "refused and successful agenda calls must leave stored rows unchanged"
        );
    }
}

#[tokio::test]
async fn agenda_from_refusals_mask_values_and_preserve_populated_paging() {
    assert_agenda_refusal("from").await;
}

#[tokio::test]
async fn agenda_to_refusals_mask_values_and_preserve_populated_paging() {
    assert_agenda_refusal("to").await;
}

#[tokio::test]
async fn agenda_after_refusals_mask_values_and_preserve_populated_paging() {
    assert_agenda_refusal("after").await;
}

#[tokio::test]
async fn agenda_after_id_refusals_mask_values_and_preserve_populated_paging() {
    assert_agenda_refusal("after_id").await;
}
