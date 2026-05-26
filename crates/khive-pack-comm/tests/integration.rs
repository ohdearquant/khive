//! Smoke tests for the comm pack (ADR-040).

use khive_pack_comm::CommPack;
use khive_runtime::{KhiveRuntime, Namespace, VerbRegistry, VerbRegistryBuilder};
use khive_types::Pack;

fn build_registry() -> (VerbRegistry, KhiveRuntime) {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(CommPack::new(runtime.clone()));
    let registry = builder.build().expect("registry builds");
    (registry, runtime)
}

/// Build a registry with a specific default namespace (for caller-scoped dispatch).
fn build_registry_for_ns(ns: &str) -> (VerbRegistry, KhiveRuntime) {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(CommPack::new(runtime.clone()));
    builder.with_default_namespace(ns);
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

#[tokio::test]
async fn test_full_id_returns_36_char() {
    let (registry, _rt) = build_registry();

    let sent = registry
        .dispatch(
            "send",
            serde_json::json!({ "to": "agent:target", "content": "hello" }),
        )
        .await
        .expect("send succeeds");

    let id = sent.get("id").and_then(|v| v.as_str()).expect("id present");
    let full_id = sent
        .get("full_id")
        .and_then(|v| v.as_str())
        .expect("full_id present");

    assert_eq!(id.len(), 8, "id must be 8-char short prefix");
    assert_eq!(full_id.len(), 36, "full_id must be 36-char hyphenated UUID");
    assert!(
        full_id.starts_with(id),
        "full_id must start with the short id prefix"
    );
    assert!(
        full_id.contains('-'),
        "full_id must be hyphenated UUID format"
    );
}

#[tokio::test]
async fn test_read_accepts_short_id() {
    let (registry, _rt) = build_registry();

    let sent = registry
        .dispatch(
            "send",
            serde_json::json!({ "to": "agent:alice", "content": "read me by short id" }),
        )
        .await
        .expect("send succeeds");

    let short = sent.get("id").and_then(|v| v.as_str()).expect("id present");

    let result = registry
        .dispatch("read", serde_json::json!({ "id": short }))
        .await
        .expect("read with 8-char short id succeeds");

    assert_eq!(
        result.get("read").and_then(|v| v.as_bool()),
        Some(true),
        "read returns read:true — got {result}"
    );
    let result_full_id = result
        .get("full_id")
        .and_then(|v| v.as_str())
        .expect("read returns full_id");
    assert_eq!(
        result_full_id.len(),
        36,
        "read response full_id must be 36-char"
    );
    assert!(
        result_full_id.starts_with(short),
        "read response full_id starts with short prefix"
    );
}

#[tokio::test]
async fn test_reply_accepts_short_id() {
    let (registry, _rt) = build_registry();

    let sent = registry
        .dispatch(
            "send",
            serde_json::json!({
                "to": "agent:carol",
                "content": "original",
                "subject": "Test"
            }),
        )
        .await
        .expect("send succeeds");

    let short = sent.get("id").and_then(|v| v.as_str()).expect("id present");

    let reply = registry
        .dispatch(
            "reply",
            serde_json::json!({ "id": short, "content": "reply via short id" }),
        )
        .await
        .expect("reply with 8-char short id succeeds");

    assert!(reply.get("id").is_some(), "reply returns id");
    let reply_full_id = reply
        .get("full_id")
        .and_then(|v| v.as_str())
        .expect("reply returns full_id");
    assert_eq!(
        reply_full_id.len(),
        36,
        "reply response full_id must be 36-char"
    );
}

#[tokio::test]
async fn test_short_id_collision_errors_clearly() {
    // Create two notes whose UUIDs share the same 8-char prefix by constructing
    // UUIDs manually and inserting them. Since we cannot control uuid::Uuid::new_v4(),
    // we verify the ambiguous-prefix error path via the runtime directly.
    //
    // Strategy: use the runtime's in-memory store to insert two notes with
    // identical 8-char prefixes, then call read with that prefix.
    use khive_runtime::KhiveRuntime;
    use khive_storage::note::Note;
    use uuid::Uuid;

    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let token = rt.authorize(khive_runtime::Namespace::local());

    // Construct two UUIDs that share the first 8 hex chars (before the first '-').
    let base = "aabbccdd";
    let uuid_a = Uuid::parse_str(&format!("{base}-1111-4000-8000-000000000001")).unwrap();
    let uuid_b = Uuid::parse_str(&format!("{base}-2222-4000-8000-000000000002")).unwrap();

    let store = rt.notes(&token).expect("notes store");
    let now = chrono::Utc::now().timestamp_micros();
    let ns = token.namespace().as_str().to_string();

    store
        .upsert_note(Note {
            id: uuid_a,
            namespace: ns.clone(),
            kind: "message".into(),
            status: "active".into(),
            name: None,
            content: "msg a".into(),
            salience: None,
            decay_factor: None,
            expires_at: None,
            properties: Some(serde_json::json!({ "direction": "inbound", "from": "x", "to": "y", "read": false })),
            created_at: now,
            updated_at: now,
            deleted_at: None,
        })
        .await
        .expect("insert a");

    store
        .upsert_note(Note {
            id: uuid_b,
            namespace: ns.clone(),
            kind: "message".into(),
            status: "active".into(),
            name: None,
            content: "msg b".into(),
            salience: None,
            decay_factor: None,
            expires_at: None,
            properties: Some(serde_json::json!({ "direction": "inbound", "from": "x", "to": "y", "read": false })),
            created_at: now,
            updated_at: now,
            deleted_at: None,
        })
        .await
        .expect("insert b");

    // Now call read with the ambiguous 8-char prefix.
    let mut builder = khive_runtime::VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(rt.clone()));
    builder.register(khive_pack_comm::CommPack::new(rt.clone()));
    let registry = builder.build().expect("registry");

    let err = registry
        .dispatch("read", serde_json::json!({ "id": base }))
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("ambiguous"),
        "ambiguous prefix error must mention 'ambiguous': got {msg:?}"
    );
}
// ── UE6 Critical F-C3: dual-write delivery tests ─────────────────────────────

/// send() from lambda:khive to lambda:leo writes one outbound note in the caller's namespace.
#[tokio::test]
async fn test_send_writes_outbound_in_caller_ns() {
    let (registry, rt) = build_registry_for_ns("lambda:khive");

    registry
        .dispatch(
            "send",
            serde_json::json!({ "to": "lambda:leo", "content": "hi" }),
        )
        .await
        .expect("send succeeds");

    // Verify: lambda:khive namespace has exactly 1 message note with direction=outbound.
    let caller_token = rt.authorize(Namespace::parse("lambda:khive").unwrap());
    let notes = rt
        .list_notes(&caller_token, Some("message"), 100, 0)
        .await
        .expect("list_notes succeeds");
    let outbound: Vec<_> = notes
        .iter()
        .filter(|n| n.deleted_at.is_none())
        .filter(|n| {
            n.properties
                .as_ref()
                .and_then(|p| p.get("direction"))
                .and_then(|v| v.as_str())
                == Some("outbound")
        })
        .collect();
    assert_eq!(
        outbound.len(),
        1,
        "caller namespace must have exactly 1 outbound note; got {outbound:?}"
    );
    assert_eq!(
        outbound[0]
            .properties
            .as_ref()
            .unwrap()
            .get("to")
            .and_then(|v| v.as_str()),
        Some("lambda:leo")
    );
}

/// send() from lambda:khive to lambda:leo writes one inbound note in the recipient's namespace.
#[tokio::test]
async fn test_send_writes_inbound_in_recipient_ns() {
    let (registry, rt) = build_registry_for_ns("lambda:khive");

    registry
        .dispatch(
            "send",
            serde_json::json!({ "to": "lambda:leo", "content": "meeting at 3pm" }),
        )
        .await
        .expect("send succeeds");

    // Verify: lambda:leo namespace has exactly 1 message note with direction=inbound.
    let recipient_token = rt.authorize(Namespace::parse("lambda:leo").unwrap());
    let notes = rt
        .list_notes(&recipient_token, Some("message"), 100, 0)
        .await
        .expect("list_notes in recipient ns succeeds");
    let inbound: Vec<_> = notes
        .iter()
        .filter(|n| n.deleted_at.is_none())
        .filter(|n| {
            n.properties
                .as_ref()
                .and_then(|p| p.get("direction"))
                .and_then(|v| v.as_str())
                == Some("inbound")
        })
        .collect();
    assert_eq!(
        inbound.len(),
        1,
        "recipient namespace must have exactly 1 inbound note; got {inbound:?}"
    );
    let props = inbound[0].properties.as_ref().unwrap();
    assert_eq!(
        props.get("from").and_then(|v| v.as_str()),
        Some("lambda:khive")
    );
    assert_eq!(props.get("to").and_then(|v| v.as_str()), Some("lambda:leo"));
    assert_eq!(inbound[0].content, "meeting at 3pm");
    // inbound copy must carry an outbound_ref back to the caller's copy.
    assert!(
        props.get("outbound_ref").is_some(),
        "inbound note must carry outbound_ref"
    );
}

/// inbox() with the recipient's MCP session returns the inbound message.
#[tokio::test]
async fn test_inbox_returns_inbound_for_recipient() {
    // Step 1: send from lambda:khive.
    let (send_registry, rt) = build_registry_for_ns("lambda:khive");
    send_registry
        .dispatch(
            "send",
            serde_json::json!({ "to": "lambda:leo", "content": "you have mail" }),
        )
        .await
        .expect("send succeeds");

    // Step 2: build a registry scoped to lambda:leo and call inbox().
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(rt.clone()));
    builder.register(CommPack::new(rt.clone()));
    builder.with_default_namespace("lambda:leo");
    let leo_registry = builder.build().expect("leo registry builds");

    let inbox = leo_registry
        .dispatch("inbox", serde_json::json!({ "status": "unread" }))
        .await
        .expect("inbox succeeds");

    let count = inbox
        .get("count")
        .and_then(|v| v.as_u64())
        .expect("inbox returns count");
    assert_eq!(
        count, 1,
        "lambda:leo inbox must have 1 unread message; got {inbox}"
    );

    let msgs = inbox.get("messages").and_then(|v| v.as_array()).unwrap();
    let props = msgs[0].get("properties").unwrap();
    assert_eq!(
        props.get("from").and_then(|v| v.as_str()),
        Some("lambda:khive")
    );
    assert_eq!(
        props.get("direction").and_then(|v| v.as_str()),
        Some("inbound")
    );
}

/// send-to-self writes exactly ONE note (no duplicate) in the caller's namespace.
#[tokio::test]
async fn test_send_to_self_writes_single_note() {
    let (registry, rt) = build_registry_for_ns("lambda:khive");

    registry
        .dispatch(
            "send",
            serde_json::json!({ "to": "lambda:khive", "content": "self-note" }),
        )
        .await
        .expect("send-to-self succeeds");

    let caller_token = rt.authorize(Namespace::parse("lambda:khive").unwrap());
    let notes = rt
        .list_notes(&caller_token, Some("message"), 100, 0)
        .await
        .expect("list_notes succeeds");
    let alive: Vec<_> = notes.iter().filter(|n| n.deleted_at.is_none()).collect();
    assert_eq!(
        alive.len(),
        1,
        "send-to-self must create exactly 1 note, not a duplicate; got {alive:?}"
    );
}

// ── UE6-H1: reply routes to the "other party", not always back to sender ────

/// Sender replies to their own outbound message → reply routes to original recipient.
///
/// A sends to B. A then replies to that message. The reply must go to B, not A.
#[tokio::test]
async fn test_reply_from_sender_routes_to_recipient() {
    // Registry scoped to lambda:khive (the sender).
    let (registry, _rt) = build_registry_for_ns("lambda:khive");

    // Send from lambda:khive to lambda:leo.
    let sent = registry
        .dispatch(
            "send",
            serde_json::json!({ "to": "lambda:leo", "content": "hello leo" }),
        )
        .await
        .expect("send succeeds");

    let msg_full_id = sent
        .get("full_id")
        .and_then(|v| v.as_str())
        .expect("send returns full_id");

    // Sender (lambda:khive) replies to their own outbound message.
    let reply = registry
        .dispatch(
            "reply",
            serde_json::json!({ "id": msg_full_id, "content": "follow-up" }),
        )
        .await
        .expect("reply succeeds");

    // Reply must route to lambda:leo (original recipient), not back to lambda:khive.
    let reply_to = reply
        .get("to")
        .and_then(|v| v.as_str())
        .expect("reply returns to");
    assert_eq!(
        reply_to, "lambda:leo",
        "UE6-H1: sender replying to own message must route to original recipient; got {reply_to}"
    );
    let reply_from = reply
        .get("from")
        .and_then(|v| v.as_str())
        .expect("reply returns from");
    assert_eq!(
        reply_from, "lambda:khive",
        "reply from must be the caller namespace"
    );
}

/// Recipient replies to an inbound message → reply routes back to original sender.
///
/// A sends to B. B replies. The reply must go to A, not B.
#[tokio::test]
async fn test_reply_from_recipient_routes_to_sender() {
    use khive_runtime::VerbRegistryBuilder;

    // Step 1: send from lambda:khive to lambda:leo.
    let (send_registry, rt) = build_registry_for_ns("lambda:khive");
    send_registry
        .dispatch(
            "send",
            serde_json::json!({ "to": "lambda:leo", "content": "meeting at 3pm" }),
        )
        .await
        .expect("send succeeds");

    // Step 2: lambda:leo reads their inbox to get the inbound message id.
    let mut leo_builder = VerbRegistryBuilder::new();
    leo_builder.register(khive_pack_kg::KgPack::new(rt.clone()));
    leo_builder.register(CommPack::new(rt.clone()));
    leo_builder.with_default_namespace("lambda:leo");
    let leo_registry = leo_builder.build().expect("leo registry");

    let inbox = leo_registry
        .dispatch("inbox", serde_json::json!({ "status": "unread" }))
        .await
        .expect("inbox succeeds");
    let msgs = inbox
        .get("messages")
        .and_then(|v| v.as_array())
        .expect("messages array");
    assert_eq!(msgs.len(), 1, "leo must have 1 inbound message");
    let inbound_full_id = msgs[0]
        .get("full_id")
        .and_then(|v| v.as_str())
        .expect("full_id on inbound message");

    // Step 3: lambda:leo replies to the inbound message.
    let reply = leo_registry
        .dispatch(
            "reply",
            serde_json::json!({ "id": inbound_full_id, "content": "confirmed" }),
        )
        .await
        .expect("reply succeeds");

    // Reply must route to lambda:khive (original sender), not back to lambda:leo.
    let reply_to = reply
        .get("to")
        .and_then(|v| v.as_str())
        .expect("reply returns to");
    assert_eq!(
        reply_to, "lambda:khive",
        "UE6-H1: recipient replying must route to original sender; got {reply_to}"
    );
    let reply_from = reply
        .get("from")
        .and_then(|v| v.as_str())
        .expect("reply returns from");
    assert_eq!(
        reply_from, "lambda:leo",
        "reply from must be the caller (lambda:leo)"
    );
}

// ── UE6-H2: reply thread_id must be full 36-char UUID ───────────────────────

/// reply thread_id must be the full 36-char hyphenated UUID of the root message.
#[tokio::test]
async fn test_reply_thread_id_is_full_uuid() {
    let (registry, _rt) = build_registry();

    let original = registry
        .dispatch(
            "send",
            serde_json::json!({ "to": "agent:target", "content": "root message" }),
        )
        .await
        .expect("send succeeds");
    let original_full_id = original
        .get("full_id")
        .and_then(|v| v.as_str())
        .expect("full_id on original");

    let reply = registry
        .dispatch(
            "reply",
            serde_json::json!({ "id": original_full_id, "content": "first reply" }),
        )
        .await
        .expect("reply succeeds");

    let thread_id = reply
        .get("thread_id")
        .and_then(|v| v.as_str())
        .expect("thread_id in reply");

    assert_eq!(
        thread_id.len(),
        36,
        "UE6-H2: thread_id must be 36-char hyphenated UUID; got {thread_id:?}"
    );
    assert!(
        thread_id.contains('-'),
        "thread_id must be hyphenated UUID format; got {thread_id:?}"
    );
    assert_eq!(
        thread_id, original_full_id,
        "thread_id must equal the original message's full UUID"
    );
    // Parse as UUID to confirm it's valid.
    thread_id
        .parse::<uuid::Uuid>()
        .unwrap_or_else(|e| panic!("thread_id must be a valid UUID: {thread_id} — {e}"));
}

/// Reply chain preserves full UUID thread_id across multiple replies.
#[tokio::test]
async fn test_reply_chain_preserves_full_uuid_thread_id() {
    let (registry, _rt) = build_registry();

    let original = registry
        .dispatch(
            "send",
            serde_json::json!({ "to": "agent:other", "content": "start of thread" }),
        )
        .await
        .expect("send succeeds");
    let original_full_id = original
        .get("full_id")
        .and_then(|v| v.as_str())
        .expect("full_id");

    // First reply — creates the thread.
    let reply1 = registry
        .dispatch(
            "reply",
            serde_json::json!({ "id": original_full_id, "content": "reply 1" }),
        )
        .await
        .expect("reply 1 succeeds");
    let thread_id_1 = reply1
        .get("thread_id")
        .and_then(|v| v.as_str())
        .expect("thread_id on reply1");
    assert_eq!(thread_id_1.len(), 36, "reply1 thread_id must be 36-char");

    // Second reply to the first reply — must carry the same root thread_id.
    let reply1_full_id = reply1
        .get("full_id")
        .and_then(|v| v.as_str())
        .expect("full_id on reply1");
    let reply2 = registry
        .dispatch(
            "reply",
            serde_json::json!({ "id": reply1_full_id, "content": "reply 2" }),
        )
        .await
        .expect("reply 2 succeeds");
    let thread_id_2 = reply2
        .get("thread_id")
        .and_then(|v| v.as_str())
        .expect("thread_id on reply2");
    assert_eq!(thread_id_2.len(), 36, "reply2 thread_id must be 36-char");
    assert_eq!(
        thread_id_1, thread_id_2,
        "all replies in a chain must share the same thread_id"
    );
}

/// inbound write failure rolls back the outbound note (atomicity).
///
/// We simulate inbound failure by passing an invalid recipient namespace string
/// (khive namespace syntax forbids control characters). The outbound note must
/// not be persisted either.
#[tokio::test]
async fn test_send_inbound_failure_rolls_back_outbound() {
    // An invalid namespace that will fail Namespace::parse.
    let invalid_recipient = "this namespace has spaces!";

    let (registry, rt) = build_registry_for_ns("lambda:khive");

    let result = registry
        .dispatch(
            "send",
            serde_json::json!({ "to": invalid_recipient, "content": "should rollback" }),
        )
        .await;

    // The send must fail because the recipient is not a valid namespace.
    assert!(
        result.is_err(),
        "send to invalid namespace must fail; got {result:?}"
    );

    // Atomicity: no outbound note should remain in lambda:khive.
    let caller_token = rt.authorize(Namespace::parse("lambda:khive").unwrap());
    let notes = rt
        .list_notes(&caller_token, Some("message"), 100, 0)
        .await
        .expect("list_notes succeeds");
    let alive: Vec<_> = notes.iter().filter(|n| n.deleted_at.is_none()).collect();
    assert_eq!(
        alive.len(),
        0,
        "failed send must not leave an outbound note in caller namespace; got {alive:?}"
    );
}
