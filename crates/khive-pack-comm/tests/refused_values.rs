use std::sync::Arc;

use khive_pack_comm::CommPack;
use khive_pack_kg::KgPack;
use khive_runtime::{
    runtime_error_value, DomainDisposition, KhiveRuntime, RuntimeConfig, RuntimeError,
    VerbRegistry, VerbRegistryBuilder,
};
use serde_json::{json, Value};

struct Conversation {
    sender: VerbRegistry,
    recipient: VerbRegistry,
    root_id: Value,
    inbound_id: Value,
}

fn actor_registry(backend: Arc<khive_db::StorageBackend>, actor: &str) -> VerbRegistry {
    let runtime = KhiveRuntime::from_backend(
        backend,
        RuntimeConfig {
            db_path: None,
            actor_id: Some(actor.into()),
            ..RuntimeConfig::no_embeddings()
        },
    );
    assert!(runtime.backend().pool().canonical_path().is_none());
    assert!(runtime.default_embedder_name().is_empty());
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some(actor.into()));
    builder.with_default_namespace("local");
    builder.register(KgPack::new(runtime.clone()));
    builder.register(CommPack::new(runtime));
    builder.build().expect("registry")
}

async fn conversation(with_reply: bool) -> Conversation {
    let backend = khive_db::StorageBackend::memory().expect("private backend");
    backend.pool().run_migrations().expect("migrations");
    let backend = Arc::new(backend);
    let sender = actor_registry(Arc::clone(&backend), "actor:sender");
    let recipient = actor_registry(backend, "actor:recipient");
    let sent = sender
        .dispatch(
            "comm.send",
            json!({
                "to":"actor:recipient", "content":"refusal root"
            }),
        )
        .await
        .expect("send fixture root");
    let inbox = recipient
        .dispatch("comm.inbox", json!({"status":"unread"}))
        .await
        .unwrap();
    assert_eq!(inbox["count"], 1);
    let inbound_id = inbox["messages"][0]["full_id"].clone();
    if with_reply {
        recipient
            .dispatch(
                "comm.reply",
                json!({
                    "id":inbound_id, "content":"refusal reply"
                }),
            )
            .await
            .expect("reply to actual inbound message");
    }
    Conversation {
        sender,
        recipient,
        root_id: sent["full_id"].clone(),
        inbound_id,
    }
}

fn refused_values() -> [(String, &'static str); 2] {
    [
        ("bogus".into(), "bogus"),
        (format!("sk-proj-{}", "A".repeat(80)), "***MASKED***"),
    ]
}

async fn assert_refusal(
    registry: &VerbRegistry,
    verb: &str,
    args: Value,
    raw: &str,
    expected: String,
) {
    let error = registry
        .dispatch(verb, args)
        .await
        .expect_err("closed-set refusal");
    let RuntimeError::InvalidInput(message) = &error else {
        panic!("{verb} changed its refusal variant");
    };
    if raw != "bogus" {
        assert!(
            !message.contains(raw),
            "{verb} exposed the synthetic credential"
        );
        assert!(message.contains("***MASKED***"), "{verb} omitted the mask");
    }
    assert_eq!(message, &expected);
    assert_eq!(
        runtime_error_value(error, DomainDisposition::NotCommitted),
        json!({
            "kind":"runtime_error", "message":format!("invalid input: {expected}"),
            "domain_disposition":"not_committed"
        })
    );
}

#[tokio::test]
async fn inbox_masks_refused_boxes_and_preserves_populated_inbox_and_sent_views() {
    let f = conversation(true).await;
    for (raw, shown) in refused_values() {
        assert_refusal(
            &f.sender,
            "comm.inbox",
            json!({"box":raw}),
            &raw,
            format!("inbox: invalid `box` {shown:?}; expected one of: inbox, sent"),
        )
        .await;
    }
    let sent = f
        .sender
        .dispatch("comm.inbox", json!({"box":"sent"}))
        .await
        .unwrap();
    assert_eq!(sent["count"], 1);
    assert_eq!(sent["messages"][0]["full_id"], f.root_id);
    assert_eq!(sent["messages"][0]["direction"], "outbound");
    let inbox = f
        .sender
        .dispatch("comm.inbox", json!({"box":"inbox", "status":"all"}))
        .await
        .unwrap();
    assert_eq!(inbox["count"], 1);
    assert_eq!(inbox["messages"][0]["content"], "refusal reply");
    assert_eq!(inbox["messages"][0]["direction"], "inbound");
}

#[tokio::test]
async fn inbox_masks_refused_statuses_without_marking_messages_read() {
    let f = conversation(false).await;
    for (raw, shown) in refused_values() {
        assert_refusal(
            &f.recipient,
            "comm.inbox",
            json!({"status":raw}),
            &raw,
            format!("inbox: invalid status {shown:?}; expected one of: unread, read, all"),
        )
        .await;
    }
    for (status, expected) in [("unread", 1), ("read", 0), ("all", 1)] {
        let inbox = f
            .recipient
            .dispatch("comm.inbox", json!({"status":status}))
            .await
            .unwrap();
        assert_eq!(inbox["count"], expected);
        if expected == 1 {
            assert_eq!(inbox["messages"][0]["full_id"], f.inbound_id);
        }
    }
    f.recipient
        .dispatch("comm.mark_read", json!({"ids":[f.inbound_id]}))
        .await
        .unwrap();
    for (status, expected) in [("unread", 0), ("read", 1), ("all", 1)] {
        let inbox = f
            .recipient
            .dispatch("comm.inbox", json!({"status":status}))
            .await
            .unwrap();
        assert_eq!(inbox["count"], expected);
        if expected == 1 {
            assert_eq!(inbox["messages"][0]["full_id"], f.inbound_id);
        }
    }
}

#[tokio::test]
async fn thread_masks_refused_orders_and_preserves_both_chronological_views() {
    let f = conversation(true).await;
    for (raw, shown) in refused_values() {
        assert_refusal(
            &f.sender,
            "comm.thread",
            json!({"id":f.root_id, "order":raw}),
            &raw,
            format!("thread: invalid order {shown:?}; expected one of: asc, desc"),
        )
        .await;
    }
    let mut ascending_ids = Vec::new();
    for order in ["asc", "desc"] {
        let thread = f
            .sender
            .dispatch("comm.thread", json!({"id":f.root_id, "order":order}))
            .await
            .unwrap();
        let rows = thread["messages"].as_array().expect("thread messages");
        assert_eq!(rows.len(), 2);
        let contents: Vec<_> = rows
            .iter()
            .map(|row| row["content"].as_str().unwrap())
            .collect();
        let ids: Vec<_> = rows.iter().map(|row| row["full_id"].clone()).collect();
        if order == "asc" {
            assert_eq!(contents, ["refusal root", "refusal reply"]);
            ascending_ids = ids;
        } else {
            assert_eq!(contents, ["refusal reply", "refusal root"]);
            ascending_ids.reverse();
            assert_eq!(ids, ascending_ids);
        }
    }
}
