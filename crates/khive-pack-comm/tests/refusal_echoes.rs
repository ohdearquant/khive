//! Refusal echoes must not disclose a detected credential, including early validation paths.
use khive_pack_comm::CommPack;
use khive_pack_kg::KgPack;
use khive_runtime::{
    runtime_error_value, DomainDisposition, KhiveRuntime, Namespace, RuntimeConfig, RuntimeError,
    VerbRegistry, VerbRegistryBuilder,
};
use serde_json::{json, Value};

fn fixture() -> (KhiveRuntime, VerbRegistry) {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        actor_id: Some("actor:refusal-reader".into()),
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap();
    assert!(runtime.backend().pool().canonical_path().is_none());
    assert!(runtime.registered_embedding_model_names().is_empty());
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some("actor:refusal-reader".into()));
    builder.register(KgPack::new(runtime.clone()));
    builder.register(CommPack::new(runtime.clone()));
    (runtime, builder.build().unwrap())
}

fn operands() -> [String; 2] {
    ["bogus".into(), format!("sk-proj-{}", "A".repeat(80))]
}

fn assert_error(error: RuntimeError, raw: &str, prefix: &str) -> String {
    let RuntimeError::InvalidInput(message) = &error else {
        panic!("wrong refusal kind: {error:?}");
    };
    assert!(message.starts_with(prefix), "{message}");
    if raw == "bogus" {
        assert!(message.contains("\"bogus\""), "{message}");
    } else {
        assert!(!message.contains(raw), "credential escaped in {message}");
        assert!(
            message.contains("***MASKED***"),
            "mask missing in {message}"
        );
    }
    let message = message.clone();
    assert_eq!(
        runtime_error_value(error, DomainDisposition::NotCommitted),
        json!({"kind":"runtime_error", "message":format!("invalid input: {message}"),
            "domain_disposition":"not_committed"})
    );
    message
}

async fn refused(registry: &VerbRegistry, verb: &str, args: Value, raw: &str, prefix: &str) {
    assert_error(
        registry.dispatch(verb, args).await.unwrap_err(),
        raw,
        prefix,
    );
}

async fn root(registry: &VerbRegistry) -> Value {
    registry
        .dispatch(
            "comm.send",
            json!({"to":"actor:other", "content":"refusal fixture"}),
        )
        .await
        .unwrap()["full_id"]
        .clone()
}

#[tokio::test]
async fn projection_refusals_cover_both_read_verbs_before_other_parameters() {
    let (_, registry) = fixture();
    let id = root(&registry).await;
    for raw in operands() {
        refused(
            &registry,
            "comm.inbox",
            json!({"fields":[raw], "box":"other"}),
            &raw,
            "inbox: unknown projection field ",
        )
        .await;
        refused(
            &registry,
            "comm.thread",
            json!({"id":id,"fields":[raw],"order":"other"}),
            &raw,
            "thread: unknown projection field ",
        )
        .await;
    }
    let inbox = registry
        .dispatch(
            "comm.inbox",
            json!({"box":"sent","fields":["full_id","content"]}),
        )
        .await
        .unwrap();
    assert_eq!(inbox["count"], 1);
    assert_eq!(
        inbox["messages"][0],
        json!({"full_id":id,"content":"refusal fixture"})
    );
    let thread = registry
        .dispatch(
            "comm.thread",
            json!({"id":id,"fields":["full_id","content"]}),
        )
        .await
        .unwrap();
    assert_eq!(thread["messages"][0], inbox["messages"][0]);
}

#[tokio::test]
async fn heartbeat_refusals_mask_closed_sets_and_timestamp_before_writing() {
    let (_, registry) = fixture();
    let base =
        json!({"channel_kind":"email","channel_slug":"refusal@example.org","outcome":"success"});
    for raw in operands() {
        for (field, prefix) in [
            ("outcome", "heartbeat: invalid `outcome` "),
            (
                "original_not_retained_reason",
                "heartbeat: unknown `original_not_retained_reason` ",
            ),
            (
                "original_retention_state",
                "heartbeat: `original_retention_state` must be ",
            ),
            ("at", "heartbeat: `at` must be a valid RFC 3339 timestamp"),
        ] {
            let mut args = base.clone();
            args[field] = json!(raw);
            if field == "original_not_retained_reason" {
                args["originals_not_retained"] = json!(1);
            }
            refused(&registry, "comm.heartbeat", args, &raw, prefix).await;
            let health = registry.dispatch("comm.health", json!({})).await.unwrap();
            assert_eq!(
                health["channels"],
                json!([]),
                "refused heartbeat wrote a row"
            );
        }
    }
    for reason in [
        "retention-limit",
        "original-unavailable",
        "retention-count-unreadable",
        "original-store-failed",
        "quarantine-write-failed",
        "secondary-backend",
    ] {
        let mut args = base.clone();
        args["originals_not_retained"] = json!(1);
        args["original_not_retained_reason"] = json!(reason);
        registry.dispatch("comm.heartbeat", args).await.unwrap();
    }
    for state in ["retained", "failing"] {
        let mut args = base.clone();
        args["original_retention_state"] = json!(state);
        args["at"] = json!("2026-10-20T10:00:00+01:00");
        registry.dispatch("comm.heartbeat", args).await.unwrap();
    }
    let mut failure = base;
    failure["outcome"] = json!("failure");
    failure["error_class"] = json!("network");
    registry.dispatch("comm.heartbeat", failure).await.unwrap();
    let health = registry.dispatch("comm.health", json!({})).await.unwrap();
    assert_eq!(health["channels"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn suppression_refusals_mask_every_closed_set_and_leave_no_records() {
    let (_, registry) = fixture();
    let base = json!({"scope":"outbound","kind":"address","value":"member@example.org","source":"operator"});
    for raw in operands() {
        for field in ["scope", "kind", "source"] {
            let mut args = base.clone();
            args[field] = json!(raw);
            refused(
                &registry,
                "comm.suppression_add",
                args,
                &raw,
                &format!("suppression_add: invalid `{field}` "),
            )
            .await;
        }
        for field in ["scope", "kind"] {
            let mut args = json!({});
            args[field] = json!(raw);
            refused(
                &registry,
                "comm.suppression_list",
                args,
                &raw,
                &format!("suppression_list: invalid `{field}` "),
            )
            .await;
        }
        let listed = registry
            .dispatch("comm.suppression_list", json!({}))
            .await
            .unwrap();
        assert_eq!(listed["items"], json!([]));
    }
    for scope in ["outbound", "inbound"] {
        for source in ["operator", "mailto_token", "one_click", "reply_stop"] {
            let mut args = base.clone();
            args["scope"] = json!(scope);
            args["source"] = json!(source);
            registry
                .dispatch("comm.suppression_add", args)
                .await
                .unwrap();
        }
    }
    for (kind, value) in [
        ("domain", "example.net"),
        (
            "recipient_key",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        ),
    ] {
        let mut args = base.clone();
        args["kind"] = json!(kind);
        args["value"] = json!(value);
        registry
            .dispatch("comm.suppression_add", args)
            .await
            .unwrap();
    }
    let listed = registry
        .dispatch("comm.suppression_list", json!({}))
        .await
        .unwrap();
    assert_eq!(listed["items"].as_array().unwrap().len(), 10);
}

#[tokio::test]
async fn inbox_timestamps_and_thread_roots_mask_values_without_changing_valid_filters() {
    let (_, registry) = fixture();
    let id = root(&registry).await;
    for raw in operands() {
        for field in ["since", "before"] {
            let mut args = json!({"box":"sent"});
            args[field] = json!(raw);
            refused(
                &registry,
                "comm.inbox",
                args,
                &raw,
                &format!("inbox: `{field}` must be a valid RFC 3339 timestamp"),
            )
            .await;
        }
        refused(
            &registry,
            "comm.inbox",
            json!({"thread_id":raw}),
            &raw,
            "inbox: `thread_id` must be a full UUID",
        )
        .await;
        refused(
            &registry,
            "comm.send",
            json!({"to":"actor:other","content":"must not write","thread_id":raw}),
            &raw,
            "send: `thread_id` must be a full UUID",
        )
        .await;
    }
    let inbox=registry.dispatch("comm.inbox",json!({"box":"sent","thread_id":id,"since":"1970-01-01T00:00:00Z","before":"2099-01-01T00:00:00Z"})).await.unwrap();
    assert_eq!(inbox["count"], 1);
    assert_eq!(inbox["messages"][0]["full_id"], id);
}

#[tokio::test]
async fn thread_id_and_after_refusals_mask_values_and_preserve_populated_order() {
    let (_, registry) = fixture();
    let id = root(&registry).await;
    let before = registry
        .dispatch("comm.thread", json!({"id":id}))
        .await
        .unwrap();
    for raw in operands() {
        refused(
            &registry,
            "comm.thread",
            json!({"id":raw}),
            &raw,
            "thread: invalid id ",
        )
        .await;
        refused(
            &registry,
            "comm.thread",
            json!({"id":id,"after":raw}),
            &raw,
            "thread: `after` cursor ",
        )
        .await;
    }
    let after = registry
        .dispatch(
            "comm.thread",
            json!({"id":id,"after":"1970-01-01T00:00:00Z"}),
        )
        .await
        .unwrap();
    assert_eq!(after["messages"], before["messages"]);
    let exhausted = registry
        .dispatch("comm.thread", json!({"id":id,"after":id}))
        .await
        .unwrap();
    assert_eq!(exhausted["messages"], json!([]));
}

#[tokio::test]
async fn shared_uuid_resolver_masks_both_error_styles_and_preserves_uuid_and_prefix_rules() {
    let (runtime, _) = fixture();
    let token = runtime.authorize(Namespace::local()).unwrap();
    for raw in operands() {
        assert_error(
            runtime
                .resolve_uuid_or_prefix(&token, &raw)
                .await
                .unwrap_err(),
            &raw,
            "invalid UUID (expected full UUID or 8+ hex prefix): ",
        );
        assert_error(
            runtime
                .resolve_uuid_or_prefix_for_verb(&token, &raw, "probe")
                .await
                .unwrap_err(),
            &raw,
            "probe: invalid id ",
        );
    }
    let uuid = uuid::Uuid::new_v4();
    assert_eq!(
        runtime
            .resolve_uuid_or_prefix(&token, &uuid.to_string())
            .await
            .unwrap(),
        uuid
    );
    for verb in [None, Some("probe")] {
        let error = match verb {
            None => runtime
                .resolve_uuid_or_prefix(&token, "abcdef12")
                .await
                .unwrap_err(),
            Some(verb) => runtime
                .resolve_uuid_or_prefix_for_verb(&token, "abcdef12", verb)
                .await
                .unwrap_err(),
        };
        let RuntimeError::InvalidInput(message) = error else {
            panic!("{error:?}")
        };
        assert_eq!(
            message,
            format!(
                "{}no record matches prefix: \"abcdef12\"",
                verb.map(|v| format!("{v}: ")).unwrap_or_default()
            )
        );
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn unknown_uuid_after_cursor_preserves_benign_echo_and_populated_thread() {
    let (runtime, registry) = fixture();
    let id = root(&registry).await;
    let root_uuid = uuid::Uuid::parse_str(id.as_str().unwrap()).unwrap();
    let raw = "ffffffff-ffff-4fff-8fff-ffffffffffff";
    let unknown = uuid::Uuid::parse_str(raw).unwrap();
    assert_eq!(unknown.to_string(), raw);
    assert_ne!(unknown, root_uuid);
    let token = runtime.authorize(Namespace::local()).unwrap();
    let notes = runtime.notes(&token).unwrap();
    let stored_before = notes.get_note(root_uuid).await.unwrap().unwrap();
    assert!(notes.get_note(unknown).await.unwrap().is_none());
    let before = registry
        .dispatch("comm.thread", json!({"id":id}))
        .await
        .unwrap();
    assert_eq!(before["count"], 1);
    assert_eq!(before["messages"].as_array().unwrap().len(), 1);
    assert_eq!(before["messages"][0]["full_id"], id);
    assert_eq!(before["messages"][0]["content"], "refusal fixture");

    let error = registry
        .dispatch("comm.thread", json!({"id":id,"after":raw}))
        .await
        .unwrap_err();
    let expected = format!("thread: `after` cursor {raw:?} does not resolve to a message");
    let RuntimeError::InvalidInput(message) = &error else {
        panic!("wrong unknown-UUID refusal kind: {error:?}");
    };
    assert_eq!(message, &expected);
    assert!(message.contains(&format!("{raw:?}")));
    assert!(!message.contains("***MASKED***"));
    assert_eq!(
        runtime_error_value(error, DomainDisposition::NotCommitted),
        json!({"kind":"runtime_error", "message":format!("invalid input: {expected}"),
            "domain_disposition":"not_committed"})
    );

    let after = registry
        .dispatch("comm.thread", json!({"id":id}))
        .await
        .unwrap();
    assert_eq!(after, before, "the populated thread must remain unchanged");
    assert_eq!(
        notes.get_note(root_uuid).await.unwrap().unwrap(),
        stored_before
    );
    assert!(notes.get_note(unknown).await.unwrap().is_none());
    let exhausted = registry
        .dispatch("comm.thread", json!({"id":id,"after":id}))
        .await
        .unwrap();
    assert_eq!(exhausted["thread_id"], id);
    assert_eq!(exhausted["count"], 0);
    assert_eq!(exhausted["messages"], json!([]));
}
