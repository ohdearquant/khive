use khive_runtime::{KhiveRuntime, Namespace, RuntimeError, VerbRegistry, VerbRegistryBuilder};
use khive_storage::Note;
use serde_json::{json, Value};
use uuid::Uuid;

fn fixture() -> (VerbRegistry, KhiveRuntime) {
    let runtime = KhiveRuntime::memory().unwrap();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(khive_pack_comm::CommPack::new(runtime.clone()));
    (builder.build().unwrap(), runtime)
}

async fn seed(registry: &VerbRegistry, content: &str) -> (Uuid, Value) {
    let outbound = registry
        .dispatch("comm.send", json!({"to": "local", "content": content}))
        .await
        .unwrap();
    let inbox = registry
        .dispatch("comm.inbox", json!({"status": "unread", "limit": 10}))
        .await
        .unwrap();
    let message = inbox["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["content"] == content)
        .unwrap();
    (
        message["full_id"].as_str().unwrap().parse().unwrap(),
        outbound,
    )
}

async fn stored(runtime: &KhiveRuntime, id: Uuid) -> Note {
    let token = runtime.authorize(Namespace::local()).unwrap();
    runtime
        .notes(&token)
        .unwrap()
        .get_note(id)
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn mark_read_scalar_alias_preserves_bulk_result_and_stored_read_in_both_modes() {
    for atomic in [false, true] {
        let (registry, runtime) = fixture();
        let (canonical_id, _) = seed(&registry, "canonical message").await;
        let (alias_id, _) = seed(&registry, "alias message").await;
        assert_eq!(
            stored(&runtime, alias_id).await.properties.unwrap()["read"],
            false
        );
        let canonical = registry
            .dispatch(
                "comm.mark_read",
                json!({"ids": [canonical_id], "atomic": atomic}),
            )
            .await
            .unwrap();
        let aliased = registry
            .dispatch("comm.mark_read", json!({"id": alias_id, "atomic": atomic}))
            .await
            .unwrap();
        for key in [
            "status",
            "requested_count",
            "unique_count",
            "marked_count",
            "failed_count",
        ] {
            assert_eq!(aliased[key], canonical[key], "{key}, atomic={atomic}");
        }
        assert_eq!(aliased["requested_count"], 1);
        assert_eq!(aliased["marked_count"], 1);
        assert_eq!(aliased["results"][0]["read"], true);
        for id in [canonical_id, alias_id] {
            assert_eq!(stored(&runtime, id).await.properties.unwrap()["read"], true);
        }
    }
}

#[tokio::test]
async fn mark_read_both_spellings_refuse_equal_different_and_null_without_mutation() {
    let (registry, runtime) = fixture();
    let (id, _) = seed(&registry, "keep unread").await;
    let before = stored(&runtime, id).await;
    for atomic in [false, true] {
        for (ids, alias) in [
            (json!([id]), json!(id)),
            (json!([id]), json!(Uuid::new_v4())),
            (Value::Null, json!(id)),
            (json!([id]), Value::Null),
            (Value::Null, Value::Null),
        ] {
            let error = registry
                .dispatch(
                    "comm.mark_read",
                    json!({"ids": ids, "id": alias, "atomic": atomic}),
                )
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains("`id` is an alias for `ids`"), "{error}");
            assert!(error.contains("supply only one"), "{error}");
            assert_eq!(stored(&runtime, id).await, before);
        }
    }
}

#[tokio::test]
async fn mark_read_alias_keeps_unknown_fields_and_scalar_types_strict() {
    let (registry, runtime) = fixture();
    let (id, _) = seed(&registry, "strict unread").await;
    let before = stored(&runtime, id).await;
    let canonical_error = registry
        .dispatch("comm.mark_read", json!({"ids": [id], "misspelled": true}))
        .await
        .unwrap_err()
        .to_string();
    let alias_error = registry
        .dispatch("comm.mark_read", json!({"id": id, "misspelled": true}))
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(alias_error, canonical_error);
    for phrase in ["unknown field `misspelled`", "`ids`", "`atomic`"] {
        assert!(alias_error.contains(phrase), "{alias_error}");
    }
    for invalid in [Value::Null, json!([id]), json!(false), json!(42)] {
        assert!(registry
            .dispatch("comm.mark_read", json!({"id": invalid}))
            .await
            .is_err());
        assert_eq!(stored(&runtime, id).await, before);
    }
}

#[tokio::test]
async fn mark_read_help_keeps_canonical_array_and_names_scalar_alias() {
    let (registry, _) = fixture();
    let help = registry
        .dispatch("comm.mark_read", json!({"help": true}))
        .await
        .unwrap();
    let params = help["params"].as_array().unwrap();
    assert!(!params.iter().any(|param| param["name"] == "id"));
    let ids = params.iter().find(|param| param["name"] == "ids").unwrap();
    assert_eq!(ids["type"], "array of string");
    assert!(ids["description"]
        .as_str()
        .unwrap()
        .contains("`id` is accepted as an alias for `ids`"));
}

#[tokio::test]
async fn delivered_listing_refusal_names_one_id_and_sent_inbox_without_changing_confirmation() {
    let (registry, runtime) = fixture();
    let (inbound, outbound) = seed(&registry, "delivery correlation").await;
    let before = stored(&runtime, inbound).await;
    let id = &outbound["full_id"];
    let confirmed = registry
        .dispatch("comm.delivered", json!({"id": id}))
        .await
        .unwrap();
    assert_eq!(confirmed["delivered"], true);
    assert_eq!(confirmed["inbound_count"], 1);
    for field in ["limit", "offset", "box", "status", "subject_contains"] {
        let mut args = json!({"id": id});
        args[field] = json!(1);
        let error = registry.dispatch("comm.delivered", args).await.unwrap_err();
        let RuntimeError::InvalidInput(error) = error else {
            panic!("listing-shaped input must retain InvalidInput: {error}");
        };
        assert!(
            error.contains(&format!("unknown field `{field}`")),
            "{error}"
        );
        assert!(error.contains("`id`"), "{error}");
        assert!(
            error.contains("comm.delivered confirms one outbound `id`"),
            "{error}"
        );
        assert!(error.contains("comm.inbox(box=\"sent\")"), "{error}");
        assert_eq!(stored(&runtime, inbound).await, before);
    }
    let missing_id = registry
        .dispatch("comm.delivered", json!({"limit": 10}))
        .await
        .unwrap_err();
    let RuntimeError::InvalidInput(missing_id) = missing_id else {
        panic!("listing without an id must retain InvalidInput");
    };
    assert!(missing_id.contains("`id`"), "{missing_id}");
    assert!(
        missing_id.contains("comm.inbox(box=\"sent\")"),
        "{missing_id}"
    );
    let unknown = registry
        .dispatch("comm.delivered", json!({"id": id, "misspelled": true}))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        unknown.contains("unknown field `misspelled`, expected `id`"),
        "{unknown}"
    );
    assert!(!unknown.contains("comm.inbox"), "{unknown}");
    let after = registry
        .dispatch("comm.delivered", json!({"id": id}))
        .await
        .unwrap();
    assert_eq!(after, confirmed);
}

#[tokio::test]
async fn mark_read_alias_retains_canonical_bounds_deduplication_and_prevalidation() {
    for atomic in [false, true] {
        let (registry, runtime) = fixture();
        let (id, outbound) = seed(&registry, "bulk boundary").await;
        let before = stored(&runtime, id).await;
        for args in [
            json!({"id": outbound["full_id"], "atomic": atomic}),
            json!({"ids": [id, outbound["full_id"]], "atomic": atomic}),
            json!({"ids": vec![id; 501], "atomic": atomic}),
        ] {
            assert!(registry.dispatch("comm.mark_read", args).await.is_err());
            assert_eq!(stored(&runtime, id).await, before);
        }
        let result = registry
            .dispatch(
                "comm.mark_read",
                json!({
                    "ids": vec![id; 500], "atomic": atomic
                }),
            )
            .await
            .unwrap();
        assert_eq!(result["requested_count"], 500);
        assert_eq!(result["unique_count"], 1);
        assert_eq!(result["marked_count"], 1);
        assert_eq!(stored(&runtime, id).await.properties.unwrap()["read"], true);
    }
}
