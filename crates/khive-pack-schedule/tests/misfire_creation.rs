//! Creation-side guarantees for ADR-106 Amendment H.

use khive_pack_schedule::{misfire::MisfirePolicy, SchedulePack};
use khive_runtime::{KhiveRuntime, Namespace, VerbRegistry, VerbRegistryBuilder};
use khive_storage::types::SqlStatement;
use serde_json::{json, Value};

mod support;

fn registry() -> (VerbRegistry, KhiveRuntime) {
    let runtime = support::memory_runtime();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(khive_pack_comm::CommPack::new(runtime.clone()));
    builder.register(SchedulePack::new(runtime.clone()));
    (builder.build().expect("registry"), runtime)
}

fn args(verb: &str, repeating: bool) -> Value {
    let mut params = json!({"at": "2099-06-01T09:00:00Z"});
    if verb == "schedule.remind" {
        params["content"] = json!("bounded reminder");
    } else {
        params["action"] = json!("create(kind=\"concept\", name=\"bounded event\")");
    }
    if repeating {
        params["repeat"] = json!("daily");
    }
    params
}

async fn note_count(runtime: &KhiveRuntime) -> u64 {
    let access = runtime.sql();
    let mut reader = access.reader().await.expect("reader");
    reader
        .count(SqlStatement {
            sql: "SELECT COUNT(*) FROM notes WHERE kind = 'scheduled_event'".into(),
            params: vec![],
            label: Some("misfire_creation_count".into()),
        })
        .await
        .expect("scheduled note count")
}

async fn properties(runtime: &KhiveRuntime, result: &Value) -> Value {
    let id = result["full_id"]
        .as_str()
        .expect("full ID")
        .parse()
        .expect("UUID");
    let token = runtime.authorize(Namespace::local()).expect("authorize");
    runtime
        .notes(&token)
        .expect("notes")
        .get_note(id)
        .await
        .expect("read note")
        .expect("stored note")
        .properties
        .expect("properties")
}

#[tokio::test]
async fn catch_up_without_explicit_bound_creates_no_note_for_either_verb_or_recurrence() {
    let (registry, runtime) = registry();
    for verb in ["schedule.remind", "schedule.schedule"] {
        for repeating in [false, true] {
            let mut params = args(verb, repeating);
            params["misfire_policy"] = json!("catch_up_once");
            let before = note_count(&runtime).await;
            let error = registry
                .dispatch(verb, params)
                .await
                .expect_err("bound required");
            assert!(
                error.to_string().contains("max_lateness"),
                "{verb}: {error}"
            );
            assert_eq!(
                note_count(&runtime).await,
                before,
                "{verb}, repeat={repeating}"
            );
        }
    }
}

#[tokio::test]
async fn malformed_policy_or_bound_is_rejected_before_any_scheduled_note_write() {
    let (registry, runtime) = registry();
    let policies = [json!("unknown"), Value::Null, json!(true), json!(7)];
    let bounds = [
        Value::Null,
        json!(0),
        json!(-1),
        json!(1.5),
        json!(true),
        json!("60"),
        json!(i64::MAX),
        json!(u64::MAX),
    ];
    for verb in ["schedule.remind", "schedule.schedule"] {
        for repeating in [false, true] {
            let mut rejected = Vec::new();
            for policy in &policies {
                let mut params = args(verb, repeating);
                params["misfire_policy"] = policy.clone();
                params["max_lateness"] = json!(60);
                rejected.push(params);
            }
            for bound in &bounds {
                let mut params = args(verb, repeating);
                params["misfire_policy"] = json!("catch_up_once");
                params["max_lateness"] = bound.clone();
                rejected.push(params);
            }
            for policy in [None, Some("skip")] {
                let mut params = args(verb, repeating);
                if let Some(policy) = policy {
                    params["misfire_policy"] = json!(policy);
                }
                params["max_lateness"] = json!(60);
                rejected.push(params);
            }
            for params in rejected {
                let before = note_count(&runtime).await;
                assert!(
                    registry.dispatch(verb, params.clone()).await.is_err(),
                    "{verb}: {params}"
                );
                assert_eq!(note_count(&runtime).await, before, "{verb}: {params}");
            }
        }
    }
}

#[tokio::test]
async fn valid_bound_is_persisted_and_omitted_policy_keeps_legacy_properties() {
    let (registry, runtime) = registry();
    for verb in ["schedule.remind", "schedule.schedule"] {
        for repeating in [false, true] {
            let legacy = registry
                .dispatch(verb, args(verb, repeating))
                .await
                .expect("legacy");
            let old = properties(&runtime, &legacy).await;
            assert!(old.get("misfire_policy").is_none());
            assert!(old.get("max_lateness").is_none());
            assert_eq!(
                khive_pack_schedule::misfire::parse_misfire_policy(&old),
                Ok(MisfirePolicy::Skip)
            );
            let mut explicit_skip = args(verb, repeating);
            explicit_skip["misfire_policy"] = json!("skip");
            let skip = registry
                .dispatch(verb, explicit_skip)
                .await
                .expect("explicit skip");
            let stored_skip = properties(&runtime, &skip).await;
            assert_eq!(stored_skip["misfire_policy"], "skip");
            assert!(stored_skip.get("max_lateness").is_none());
            let mut params = args(verb, repeating);
            params["misfire_policy"] = json!("catch_up_once");
            params["max_lateness"] = json!(60);
            let result = registry
                .dispatch(verb, params)
                .await
                .expect("bounded catch-up");
            let stored = properties(&runtime, &result).await;
            assert_eq!(stored["misfire_policy"], "catch_up_once");
            assert_eq!(stored["max_lateness"], 60);
            assert_eq!(stored["status"], "pending");
            assert_eq!(
                khive_pack_schedule::misfire::parse_misfire_policy(&stored),
                Ok(MisfirePolicy::CatchUpOnce { max_lateness: 60 })
            );
        }
    }
}
