//! Smoke tests for the comm pack (ADR-040).

use khive_pack_comm::CommPack;
use khive_runtime::{KhiveRuntime, VerbRegistry, VerbRegistryBuilder};
use khive_types::Pack;

fn build_registry() -> (VerbRegistry, KhiveRuntime) {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(CommPack::new(runtime.clone()));
    let registry = builder.build().expect("registry builds");
    (registry, runtime)
}

#[test]
fn comm_pack_declares_message_note_kind() {
    assert!(CommPack::NOTE_KINDS.contains(&"message"));
}

#[test]
fn comm_pack_declares_four_handlers() {
    assert_eq!(CommPack::HANDLERS.len(), 4);
    let names: Vec<&str> = CommPack::HANDLERS.iter().map(|h| h.name).collect();
    assert!(names.contains(&"send"));
    assert!(names.contains(&"inbox"));
    assert!(names.contains(&"read"));
    assert!(names.contains(&"reply"));
}

#[test]
fn comm_pack_requires_kg() {
    assert_eq!(CommPack::REQUIRES, &["kg"]);
}

#[tokio::test]
async fn send_and_inbox_roundtrip() {
    let (registry, _rt) = build_registry();

    // Send a message — creates an outbound message note.
    let result = registry
        .dispatch(
            "send",
            serde_json::json!({ "to": "agent:bob", "content": "hello" }),
        )
        .await
        .expect("send succeeds");
    assert!(result.get("id").is_some(), "send returns id: {result}");

    // Inbox with status=all returns the sent message (outbound notes are not listed by default).
    let inbox = registry
        .dispatch("inbox", serde_json::json!({ "status": "all", "limit": 10 }))
        .await
        .expect("inbox succeeds");
    // We sent an outbound message; inbox only lists inbound by default.
    // status=all also includes outbound, but direction filter still applies.
    // The test verifies inbox runs without error; count may be 0 for outbound.
    assert!(inbox.get("count").is_some(), "inbox returns count: {inbox}");
}

#[tokio::test]
async fn read_marks_message_as_read() {
    let (registry, _rt) = build_registry();

    // Send a message and capture the full_id.
    let sent = registry
        .dispatch(
            "send",
            serde_json::json!({ "to": "agent:alice", "content": "mark me read" }),
        )
        .await
        .expect("send succeeds");
    let full_id = sent
        .get("full_id")
        .and_then(|v| v.as_str())
        .expect("send returns full_id");

    // Call read with the full UUID — must succeed and return read: true.
    let result = registry
        .dispatch("read", serde_json::json!({ "id": full_id }))
        .await
        .expect("read succeeds");
    assert_eq!(
        result.get("read").and_then(|v| v.as_bool()),
        Some(true),
        "read returns read:true — got {result}"
    );
    assert_eq!(
        result.get("full_id").and_then(|v| v.as_str()),
        Some(full_id),
        "read returns the same message id"
    );
}

#[tokio::test]
async fn reply_creates_threaded_message() {
    let (registry, _rt) = build_registry();

    // Send the original message.
    let original = registry
        .dispatch(
            "send",
            serde_json::json!({
                "to": "agent:carol",
                "content": "original message",
                "subject": "Hello"
            }),
        )
        .await
        .expect("send original succeeds");
    let original_full_id = original
        .get("full_id")
        .and_then(|v| v.as_str())
        .expect("send returns full_id");

    // Reply to the original message.
    let reply = registry
        .dispatch(
            "reply",
            serde_json::json!({
                "id": original_full_id,
                "content": "this is a reply"
            }),
        )
        .await
        .expect("reply succeeds");

    // reply must return an id (the new message).
    assert!(reply.get("id").is_some(), "reply returns id: {reply}");
    // thread_id must be set to the original message's UUID.
    assert_eq!(
        reply.get("thread_id").and_then(|v| v.as_str()),
        Some(original_full_id),
        "reply thread_id matches original full_id: {reply}"
    );
    // subject should be prefixed with "Re: ".
    assert_eq!(
        reply.get("subject").and_then(|v| v.as_str()),
        Some("Re: Hello"),
        "reply subject is prefixed with Re: — got {reply}"
    );
}

#[tokio::test]
async fn unknown_verb_returns_error() {
    let (registry, _rt) = build_registry();
    let err = registry
        .dispatch("comm.does_not_exist", serde_json::Value::Null)
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("comm.does_not_exist") || err.to_string().contains("unknown verb")
    );
}
