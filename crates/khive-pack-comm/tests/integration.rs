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
fn comm_pack_declares_five_handlers() {
    assert_eq!(
        CommPack::HANDLERS.len(),
        5,
        "comm pack must declare 5 handlers: send, inbox, read, reply, thread"
    );
    let names: Vec<&str> = CommPack::HANDLERS.iter().map(|h| h.name).collect();
    assert!(names.contains(&"comm.send"));
    assert!(names.contains(&"comm.inbox"));
    assert!(names.contains(&"comm.read"));
    assert!(names.contains(&"comm.reply"));
    assert!(
        names.contains(&"comm.thread"),
        "comm.thread verb must be registered"
    );
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
            "comm.send",
            serde_json::json!({ "to": "agent:bob", "content": "hello" }),
        )
        .await
        .expect("send succeeds");
    assert!(result.get("id").is_some(), "send returns id: {result}");

    // Inbox with status=all returns the sent message (outbound notes are not listed by default).
    let inbox = registry
        .dispatch(
            "comm.inbox",
            serde_json::json!({ "status": "all", "limit": 10 }),
        )
        .await
        .expect("inbox succeeds");
    // We sent an outbound message; inbox only lists inbound by default.
    // status=all also includes outbound, but direction filter still applies.
    // The test verifies inbox runs without error; count may be 0 for outbound.
    assert!(inbox.get("count").is_some(), "inbox returns count: {inbox}");
}

#[tokio::test]
async fn read_marks_message_as_read() {
    let (registry, rt) = build_registry_for_ns("local");

    // Send to self so both an outbound AND an inbound copy land in the same
    // "local" namespace. read() is only valid on inbound messages.
    registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "local", "content": "mark me read" }),
        )
        .await
        .expect("send succeeds");

    // Find the inbound copy in the caller namespace.
    let caller_token = rt.authorize(khive_runtime::Namespace::parse("local").unwrap());
    let notes = rt
        .list_notes(&caller_token, Some("message"), 100, 0)
        .await
        .expect("list_notes");
    let inbound_note = notes
        .iter()
        .find(|n| {
            n.deleted_at.is_none()
                && n.properties
                    .as_ref()
                    .and_then(|p| p.get("direction"))
                    .and_then(|v| v.as_str())
                    == Some("inbound")
        })
        .expect("inbound copy must exist after self-send");
    let inbound_full_id = inbound_note.id.to_string();

    // Call read with the inbound UUID — must succeed and return read: true.
    let result = registry
        .dispatch("comm.read", serde_json::json!({ "id": inbound_full_id }))
        .await
        .expect("read on inbound message succeeds");
    assert_eq!(
        result.get("read").and_then(|v| v.as_bool()),
        Some(true),
        "read returns read:true — got {result}"
    );
    assert_eq!(
        result.get("full_id").and_then(|v| v.as_str()),
        Some(inbound_full_id.as_str()),
        "read returns the same message id"
    );
}

#[tokio::test]
async fn reply_creates_threaded_message() {
    let (registry, _rt) = build_registry();

    // Send the original message.
    let original = registry
        .dispatch(
            "comm.send",
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
            "comm.reply",
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
            "comm.send",
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
    // Send to self so the inbound copy lands in the same "local" namespace.
    // read() is only valid on inbound messages.
    let (registry, rt) = build_registry_for_ns("local");

    registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "local", "content": "read me by short id" }),
        )
        .await
        .expect("send succeeds");

    // Locate the inbound copy.
    let caller_token = rt.authorize(khive_runtime::Namespace::parse("local").unwrap());
    let notes = rt
        .list_notes(&caller_token, Some("message"), 100, 0)
        .await
        .expect("list_notes");
    let inbound = notes
        .iter()
        .find(|n| {
            n.deleted_at.is_none()
                && n.properties
                    .as_ref()
                    .and_then(|p| p.get("direction"))
                    .and_then(|v| v.as_str())
                    == Some("inbound")
        })
        .expect("inbound copy must exist after self-send");
    let inbound_short = &inbound.id.to_string()[..8];

    let result = registry
        .dispatch("comm.read", serde_json::json!({ "id": inbound_short }))
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
        result_full_id.starts_with(inbound_short),
        "read response full_id starts with short prefix"
    );
}

#[tokio::test]
async fn test_reply_accepts_short_id() {
    let (registry, _rt) = build_registry();

    let sent = registry
        .dispatch(
            "comm.send",
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
            "comm.reply",
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
        .dispatch("comm.read", serde_json::json!({ "id": base }))
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
            "comm.send",
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
            "comm.send",
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
            "comm.send",
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
        .dispatch("comm.inbox", serde_json::json!({ "status": "unread" }))
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

/// send-to-self writes exactly TWO notes (one outbound, one inbound) in the caller's
/// namespace.  The inbound copy is required so that `inbox()` can surface the message
/// to the sender when they are also the recipient.
#[tokio::test]
async fn test_send_to_self_writes_two_notes() {
    let (registry, rt) = build_registry_for_ns("lambda:khive");

    registry
        .dispatch(
            "comm.send",
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
        2,
        "send-to-self must create exactly 2 notes (outbound + inbound copy); got {alive:?}"
    );
    let directions: Vec<&str> = alive
        .iter()
        .filter_map(|n| {
            n.properties
                .as_ref()
                .and_then(|p| p.get("direction"))
                .and_then(|v| v.as_str())
        })
        .collect();
    assert!(
        directions.contains(&"outbound"),
        "self-send must include an outbound note; got {directions:?}"
    );
    assert!(
        directions.contains(&"inbound"),
        "self-send must include an inbound note for inbox visibility; got {directions:?}"
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
            "comm.send",
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
            "comm.reply",
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
            "comm.send",
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
        .dispatch("comm.inbox", serde_json::json!({ "status": "unread" }))
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
            "comm.reply",
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
            "comm.send",
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
            "comm.reply",
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
            "comm.send",
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
            "comm.reply",
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
            "comm.reply",
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
            "comm.send",
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

// ── CC-2 C3 regression: inbox() returns self-sent messages ───────────────────

/// After a self-send, inbox(status="all") must return at least the inbound copy.
/// Before the fix, inbox always returned 0 for self-sends because no inbound
/// note was written.
#[tokio::test]
async fn test_inbox_returns_self_send_as_inbound() {
    let (registry, _rt) = build_registry_for_ns("local");

    registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "local", "content": "self message for inbox" }),
        )
        .await
        .expect("self-send succeeds");

    let inbox = registry
        .dispatch("comm.inbox", serde_json::json!({ "status": "all" }))
        .await
        .expect("inbox succeeds");

    let count = inbox
        .get("count")
        .and_then(|v| v.as_u64())
        .expect("inbox returns count");
    assert!(
        count >= 1,
        "CC-2 C3 regression: inbox(status=all) must return at least 1 message after self-send; got count={count}"
    );

    // Verify the message is marked as inbound.
    let msgs = inbox.get("messages").and_then(|v| v.as_array()).unwrap();
    assert!(
        msgs.iter().any(|m| m
            .get("properties")
            .and_then(|p| p.get("direction"))
            .and_then(|v| v.as_str())
            == Some("inbound")),
        "CC-2 C3 regression: inbox must contain an inbound message; got {inbox}"
    );
}

// ── CC-2 C1 regression: list(kind=message, thread_id=X) filters correctly ────

/// list(kind="message", thread_id=X) must return only messages in that thread.
/// Before the fix, thread_id was silently ignored and all messages were returned.
#[tokio::test]
async fn test_list_message_thread_id_filter() {
    let (send_registry, rt) = build_registry_for_ns("lambda:khive");

    // Send two messages — one with a thread_id, one without.
    let msg1 = send_registry
        .dispatch(
            "comm.send",
            serde_json::json!({
                "to": "lambda:khive",
                "content": "threaded message",
                "thread_id": "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"
            }),
        )
        .await
        .expect("send msg1 succeeds");
    let _thread_id = msg1
        .get("full_id")
        .and_then(|v| v.as_str())
        .expect("msg1 full_id");

    send_registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:khive", "content": "unthreaded message" }),
        )
        .await
        .expect("send msg2 succeeds");

    // Build a kg-scoped registry in the same ns for list() (list is a KG verb).
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(rt.clone()));
    builder.register(CommPack::new(rt.clone()));
    builder.with_default_namespace("lambda:khive");
    let list_registry = builder.build().expect("list registry builds");

    let result = list_registry
        .dispatch(
            "list",
            serde_json::json!({
                "kind": "message",
                "thread_id": "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"
            }),
        )
        .await
        .expect("list with thread_id filter succeeds");

    let items = result.as_array().expect("list returns an array");
    // Every returned message must have the requested thread_id.
    for item in items {
        let stored_thread = item
            .get("properties")
            .and_then(|p| p.get("thread_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert_eq!(
            stored_thread, "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
            "CC-2 C1 regression: list(thread_id=X) must only return messages in that thread; got {item}"
        );
    }
}

// ── CC-2 C2 regression: list(kind=message, direction=inbound) filters ────────

/// list(kind="message", direction="inbound") must return only inbound messages.
/// Before the fix, direction was silently ignored and all messages were returned.
#[tokio::test]
async fn test_list_message_direction_filter() {
    let (registry, rt) = build_registry_for_ns("local");

    // Self-send creates one outbound and one inbound copy.
    registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "local", "content": "direction test" }),
        )
        .await
        .expect("self-send succeeds");

    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(rt.clone()));
    builder.register(CommPack::new(rt.clone()));
    builder.with_default_namespace("local");
    let list_registry = builder.build().expect("list registry builds");

    // Filter for inbound only.
    let inbound = list_registry
        .dispatch(
            "list",
            serde_json::json!({ "kind": "message", "direction": "inbound" }),
        )
        .await
        .expect("list(direction=inbound) succeeds");
    let inbound_items = inbound.as_array().expect("list returns array");
    assert!(
        !inbound_items.is_empty(),
        "CC-2 C2 regression: list(direction=inbound) must return at least 1 message; got empty"
    );
    for item in inbound_items {
        let dir = item
            .get("properties")
            .and_then(|p| p.get("direction"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert_eq!(
            dir, "inbound",
            "CC-2 C2 regression: list(direction=inbound) must only return inbound; got {item}"
        );
    }

    // Filter for outbound only.
    let outbound = list_registry
        .dispatch(
            "list",
            serde_json::json!({ "kind": "message", "direction": "outbound" }),
        )
        .await
        .expect("list(direction=outbound) succeeds");
    let outbound_items = outbound.as_array().expect("list returns array");
    assert!(
        !outbound_items.is_empty(),
        "CC-2 C2 regression: list(direction=outbound) must return at least 1 message; got empty"
    );
    for item in outbound_items {
        let dir = item
            .get("properties")
            .and_then(|p| p.get("direction"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert_eq!(
            dir, "outbound",
            "CC-2 C2 regression: list(direction=outbound) must only return outbound; got {item}"
        );
    }
}

// ── ue-comm-sched C2 regression: read() rejects outbound messages ─────────────

/// read() on an outbound message must return an error.
/// Before the fix, read() silently mutated outbound messages, corrupting
/// the read/unread invariant.
#[tokio::test]
async fn test_read_rejects_outbound_message() {
    let (registry, _rt) = build_registry_for_ns("lambda:khive");

    // Send cross-namespace — the outbound copy stays in lambda:khive.
    let sent = registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:leo", "content": "outbound read attempt" }),
        )
        .await
        .expect("send succeeds");

    let outbound_full_id = sent
        .get("full_id")
        .and_then(|v| v.as_str())
        .expect("send returns full_id");

    // read() on the outbound copy must be rejected.
    let result = registry
        .dispatch("comm.read", serde_json::json!({ "id": outbound_full_id }))
        .await;

    assert!(
        result.is_err(),
        "ue-comm-sched C2 regression: read() on outbound message must fail; got ok"
    );
    let err_msg = format!("{}", result.unwrap_err());
    assert!(
        err_msg.contains("outbound") || err_msg.contains("direction"),
        "ue-comm-sched C2: error must mention outbound/direction; got {err_msg}"
    );
}

// ── H3 regression: thread verb is registered and returns thread messages ──────

/// thread(id=X) must return all messages in the thread in chronological order.
/// Before the fix, the thread verb was not registered, causing "unknown verb" error.
#[tokio::test]
async fn test_thread_verb_returns_threaded_messages() {
    let (registry, _rt) = build_registry_for_ns("local");

    // Send the root message to self.
    let root = registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "local", "content": "thread root" }),
        )
        .await
        .expect("root send succeeds");

    let root_full_id = root
        .get("full_id")
        .and_then(|v| v.as_str())
        .expect("root full_id");

    // Reply to create a threaded child.
    registry
        .dispatch(
            "comm.reply",
            serde_json::json!({ "id": root_full_id, "content": "thread reply" }),
        )
        .await
        .expect("reply succeeds");

    // Thread verb must return at least the root + the reply.
    let thread_result = registry
        .dispatch("comm.thread", serde_json::json!({ "id": root_full_id }))
        .await
        .expect("H3 regression: thread verb must be registered");

    let count = thread_result
        .get("count")
        .and_then(|v| v.as_u64())
        .expect("thread returns count");
    assert!(
        count >= 2,
        "H3 regression: thread must return root + reply (at least 2); got count={count}, result={thread_result}"
    );

    let msgs = thread_result
        .get("messages")
        .and_then(|v| v.as_array())
        .expect("thread returns messages array");
    // Messages must be in chronological order (created_at ascending).
    let timestamps: Vec<i64> = msgs
        .iter()
        .filter_map(|m| m.get("created_at").and_then(|v| v.as_i64()))
        .collect();
    let mut sorted = timestamps.clone();
    sorted.sort_unstable();
    assert_eq!(
        timestamps, sorted,
        "H3: thread must return messages in chronological order"
    );
}

// ── High 1 regression: reply() delivers inbound copy to recipient namespace ───

/// reply() must write an inbound copy to the original sender's namespace, not
/// just an outbound note in the caller's namespace.
///
/// Before the fix, reply() created only an outbound note via a single
/// create_note call, so the original sender never received the reply in inbox().
#[tokio::test]
async fn test_reply_delivers_inbound_to_recipient() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");

    // lambda:khive sends to lambda:leo.
    let mut khive_builder = VerbRegistryBuilder::new();
    khive_builder.register(khive_pack_kg::KgPack::new(rt.clone()));
    khive_builder.register(CommPack::new(rt.clone()));
    khive_builder.with_default_namespace("lambda:khive");
    let khive_reg = khive_builder.build().expect("khive registry");

    khive_reg
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:leo", "content": "original from khive" }),
        )
        .await
        .expect("send succeeds");

    // lambda:leo reads inbox and replies.
    let mut leo_builder = VerbRegistryBuilder::new();
    leo_builder.register(khive_pack_kg::KgPack::new(rt.clone()));
    leo_builder.register(CommPack::new(rt.clone()));
    leo_builder.with_default_namespace("lambda:leo");
    let leo_reg = leo_builder.build().expect("leo registry");

    let inbox = leo_reg
        .dispatch("comm.inbox", serde_json::json!({ "status": "all" }))
        .await
        .expect("leo inbox succeeds");
    let msgs = inbox.get("messages").and_then(|v| v.as_array()).unwrap();
    assert_eq!(msgs.len(), 1, "leo must have 1 inbound message");
    let inbound_id = msgs[0].get("full_id").and_then(|v| v.as_str()).unwrap();

    leo_reg
        .dispatch(
            "comm.reply",
            serde_json::json!({ "id": inbound_id, "content": "reply from leo" }),
        )
        .await
        .expect("reply succeeds");

    // lambda:khive must see the reply in their inbox.
    let khive_inbox = khive_reg
        .dispatch("comm.inbox", serde_json::json!({ "status": "all" }))
        .await
        .expect("khive inbox after reply succeeds");
    let khive_count = khive_inbox
        .get("count")
        .and_then(|v| v.as_u64())
        .expect("count field");
    assert!(
        khive_count >= 1,
        "High-1 regression: reply() must deliver an inbound copy to the original sender; \
         lambda:khive inbox count={khive_count} (expected >= 1)"
    );

    // The inbound copy in khive must be direction=inbound.
    let khive_msgs = khive_inbox
        .get("messages")
        .and_then(|v| v.as_array())
        .unwrap();
    assert!(
        khive_msgs.iter().any(|m| m
            .get("properties")
            .and_then(|p| p.get("direction"))
            .and_then(|v| v.as_str())
            == Some("inbound")),
        "High-1 regression: inbound copy in lambda:khive must have direction=inbound; \
         got {khive_inbox}"
    );
}

// ── High 3 regression: thread() rejects nonexistent or non-message root ────────

/// thread(id=X) with a nonexistent UUID must return an error, not a silent empty result.
/// Before the fix, thread() accepted any resolvable UUID and returned Ok with count=0.
#[tokio::test]
async fn test_thread_rejects_nonexistent_root() {
    let (registry, _rt) = build_registry_for_ns("local");

    // A freshly-generated UUID that was never stored.
    let phantom_id = uuid::Uuid::new_v4().to_string();
    let result = registry
        .dispatch("comm.thread", serde_json::json!({ "id": phantom_id }))
        .await;

    assert!(
        result.is_err(),
        "High-3 regression: thread() with nonexistent root UUID must return an error; \
         got ok with result={result:?}"
    );
}

/// thread(id=X) where X is a non-message note must return an error.
#[tokio::test]
async fn test_thread_rejects_non_message_root() {
    let (registry, rt) = build_registry_for_ns("local");

    // Create a non-message note (kind=observation) using the KG verb.
    let obs = registry
        .dispatch(
            "create",
            serde_json::json!({ "kind": "observation", "content": "not a message" }),
        )
        .await
        .expect("create observation succeeds");
    let obs_full_id = obs
        .get("full_id")
        .or_else(|| obs.get("id"))
        .and_then(|v| v.as_str())
        .expect("observation has id");

    // Resolve the short id to full UUID if needed.
    let full_id = if obs_full_id.len() == 8 {
        // Need to get the full UUID from the note store.
        let tok = rt.authorize(khive_runtime::Namespace::parse("local").unwrap());
        let notes = rt
            .list_notes(&tok, Some("observation"), 10, 0)
            .await
            .expect("list observations");
        notes
            .first()
            .map(|n| n.id.as_hyphenated().to_string())
            .unwrap_or_else(|| obs_full_id.to_string())
    } else {
        obs_full_id.to_string()
    };

    let result = registry
        .dispatch("comm.thread", serde_json::json!({ "id": full_id }))
        .await;

    assert!(
        result.is_err(),
        "High-3 regression: thread() with non-message root must return an error; \
         got ok with result={result:?}"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("message") || err.contains("kind"),
        "High-3: error must mention 'message' or 'kind'; got {err}"
    );
}

// ── Medium regression: inbox paginated scan works past the old prefetch window ─

/// inbox() must return matching inbound messages even when more than the old
/// prefetch window (limit*4) of non-matching messages precede them.
///
/// Before the fix, inbox() fetched at most limit*4 notes and applied in-memory
/// filtering — if all newest notes were outbound, older inbound messages were
/// invisible. This test creates 25 outbound-only messages before the inbound
/// message to push it outside the old window.
#[tokio::test]
async fn test_inbox_paginated_scan_finds_message_beyond_prefetch_window() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");

    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(rt.clone()));
    builder.register(CommPack::new(rt.clone()));
    builder.with_default_namespace("local");
    let registry = builder.build().expect("registry");

    // Send 1 self-send (creates both inbound and outbound copies).
    registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "local", "content": "the important inbound message" }),
        )
        .await
        .expect("first send succeeds");

    // Now send 25 cross-namespace messages — these produce outbound copies in "local"
    // but inbound copies in "lambda:other".  The "local" namespace then has 25 outbound
    // notes that post-date the original inbound copy.
    for i in 0..25u32 {
        // Cross-namespace send: outbound stays in "local", inbound goes to "lambda:other".
        // We need a second runtime/registry scoped to "local" to write outbound notes.
        let tok = rt.authorize(khive_runtime::Namespace::parse("local").unwrap());
        let _ = rt
            .create_note(
                &tok,
                "message",
                None,
                &format!("noise outbound message {i}"),
                None,
                Some(serde_json::json!({
                    "from": "local",
                    "to": "lambda:other",
                    "direction": "outbound",
                    "read": false,
                    "sent_at": chrono::Utc::now().to_rfc3339(),
                })),
                vec![],
            )
            .await
            .expect("noise send succeeds");
    }

    // With default limit=5, the old code fetched limit*4=20 notes (all outbound noise)
    // and would return 0 inbound messages.  The paginated scan must find the 1 inbound.
    let inbox = registry
        .dispatch(
            "comm.inbox",
            serde_json::json!({ "status": "all", "limit": 5 }),
        )
        .await
        .expect("inbox succeeds");

    let count = inbox
        .get("count")
        .and_then(|v| v.as_u64())
        .expect("inbox returns count");
    assert!(
        count >= 1,
        "Medium regression: inbox() must find inbound message even when preceded by \
         more than limit*4 outbound messages; got count={count}, inbox={inbox}"
    );
}

// ── Round-3 regressions: inbox limit schema + invalid status ────────────────

/// inbox(limit=200) must succeed — 200 is the documented and enforced maximum.
#[tokio::test]
async fn test_inbox_limit_200_succeeds() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(rt.clone()));
    builder.register(CommPack::new(rt.clone()));
    builder.with_default_namespace("local");
    let registry = builder.build().expect("registry");

    // Provide one message so the store is non-empty.
    registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "local", "content": "ping" }),
        )
        .await
        .expect("send succeeds");

    let result = registry
        .dispatch(
            "comm.inbox",
            serde_json::json!({ "limit": 200, "status": "all" }),
        )
        .await;
    assert!(
        result.is_ok(),
        "inbox(limit=200) must succeed; got err={:?}",
        result.unwrap_err()
    );
}

/// inbox(limit=201) clamps silently to 200 and succeeds — no InvalidInput.
#[tokio::test]
async fn test_inbox_limit_201_clamps_to_200() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(rt.clone()));
    builder.register(CommPack::new(rt.clone()));
    builder.with_default_namespace("local");
    let registry = builder.build().expect("registry");

    registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "local", "content": "ping" }),
        )
        .await
        .expect("send succeeds");

    // The handler uses .clamp(1, 200), so 201 is silently capped — not rejected.
    let result = registry
        .dispatch(
            "comm.inbox",
            serde_json::json!({ "limit": 201, "status": "all" }),
        )
        .await;
    assert!(
        result.is_ok(),
        "inbox(limit=201) must clamp silently to 200, not return an error; got err={:?}",
        result.unwrap_err()
    );
}

/// inbox(status="banana") must return InvalidInput — unknown status values are rejected.
#[tokio::test]
async fn test_inbox_invalid_status_banana_rejected() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(rt.clone()));
    builder.register(CommPack::new(rt.clone()));
    builder.with_default_namespace("local");
    let registry = builder.build().expect("registry");

    let result = registry
        .dispatch("comm.inbox", serde_json::json!({ "status": "banana" }))
        .await;
    assert!(
        result.is_err(),
        "inbox(status=\"banana\") must return an error; got ok={:?}",
        result.unwrap()
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("banana") || err.contains("InvalidInput") || err.contains("invalid"),
        "error must mention the bad value or InvalidInput; got {err}"
    );
}

// ── H1 regression: cross-namespace thread query ───────────────────────────────

/// A sends to B, B replies, A queries comm.thread(id=A's outbound UUID) — must
/// return both A's outbound and B's reply.
///
/// Before the fix, A's outbound copy and B's inbound copy had `thread_id=None`
/// (root).  When B replied, the reply's thread_id was set to `id_B` (B's local
/// copy UUID), not to `id_A`.  A's thread query would miss B's reply because it
/// searched for `thread_id == id_A`.
///
/// After the fix, dual_write_message stamps BOTH copies with the same canonical
/// thread_id (the outbound UUID `id_A`), so all replies from any namespace carry
/// `thread_id = id_A` and A's thread query finds them.
#[tokio::test]
async fn test_cross_namespace_thread_query_finds_reply() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");

    // lambda:khive sends to lambda:leo.
    let mut khive_builder = VerbRegistryBuilder::new();
    khive_builder.register(khive_pack_kg::KgPack::new(rt.clone()));
    khive_builder.register(CommPack::new(rt.clone()));
    khive_builder.with_default_namespace("lambda:khive");
    let khive_reg = khive_builder.build().expect("khive registry");

    let sent = khive_reg
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:leo", "content": "hello from khive" }),
        )
        .await
        .expect("send succeeds");

    let outbound_full_id = sent
        .get("full_id")
        .and_then(|v| v.as_str())
        .expect("send returns full_id");

    // lambda:leo reads inbox and gets the inbound copy UUID (id_B).
    let mut leo_builder = VerbRegistryBuilder::new();
    leo_builder.register(khive_pack_kg::KgPack::new(rt.clone()));
    leo_builder.register(CommPack::new(rt.clone()));
    leo_builder.with_default_namespace("lambda:leo");
    let leo_reg = leo_builder.build().expect("leo registry");

    let inbox = leo_reg
        .dispatch("comm.inbox", serde_json::json!({ "status": "all" }))
        .await
        .expect("leo inbox succeeds");
    let msgs = inbox.get("messages").and_then(|v| v.as_array()).unwrap();
    assert_eq!(msgs.len(), 1, "leo must have 1 inbound message");
    let inbound_full_id = msgs[0]
        .get("full_id")
        .and_then(|v| v.as_str())
        .expect("inbound full_id");

    // Both copies must share the same canonical thread_id (id_A = outbound_full_id).
    let inbound_thread_id = msgs[0]
        .get("properties")
        .and_then(|p| p.get("thread_id"))
        .and_then(|v| v.as_str())
        .expect("inbound copy must have thread_id");
    assert_eq!(
        inbound_thread_id, outbound_full_id,
        "H1: inbound copy thread_id must equal outbound UUID (canonical root); \
         inbound_full_id={inbound_full_id} outbound_full_id={outbound_full_id} \
         inbound_thread_id={inbound_thread_id}"
    );

    // lambda:leo replies to the inbound copy (id_B).
    leo_reg
        .dispatch(
            "comm.reply",
            serde_json::json!({ "id": inbound_full_id, "content": "reply from leo" }),
        )
        .await
        .expect("reply succeeds");

    // lambda:khive queries comm.thread(id=outbound_full_id=id_A).
    // Must return at least: A's outbound + B's reply inbound (delivered to lambda:khive).
    let thread_result = khive_reg
        .dispatch("comm.thread", serde_json::json!({ "id": outbound_full_id }))
        .await
        .expect("H1: thread query from A's namespace must succeed");

    let count = thread_result
        .get("count")
        .and_then(|v| v.as_u64())
        .expect("thread returns count");
    assert!(
        count >= 2,
        "H1 regression: comm.thread(id=outbound_id) must return at least 2 messages \
         (A's outbound + B's reply); got count={count}, result={thread_result}"
    );
}

/// comm.thread resolves correctly when called with the inbound copy UUID (id_B)
/// instead of the outbound UUID (id_A).
#[tokio::test]
async fn test_thread_resolves_from_inbound_copy_uuid() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");

    let mut khive_builder = VerbRegistryBuilder::new();
    khive_builder.register(khive_pack_kg::KgPack::new(rt.clone()));
    khive_builder.register(CommPack::new(rt.clone()));
    khive_builder.with_default_namespace("lambda:khive");
    let khive_reg = khive_builder.build().expect("khive registry");

    // Self-send so both copies land in the same namespace.
    let sent = khive_reg
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:khive", "content": "self root message" }),
        )
        .await
        .expect("self-send succeeds");
    let outbound_full_id = sent
        .get("full_id")
        .and_then(|v| v.as_str())
        .expect("outbound full_id");

    // Find the inbound copy (direction=inbound) — it has a different UUID.
    let caller_token = rt.authorize(Namespace::parse("lambda:khive").unwrap());
    let notes = rt
        .list_notes(&caller_token, Some("message"), 100, 0)
        .await
        .expect("list_notes");
    let inbound_note = notes
        .iter()
        .find(|n| {
            n.deleted_at.is_none()
                && n.properties
                    .as_ref()
                    .and_then(|p| p.get("direction"))
                    .and_then(|v| v.as_str())
                    == Some("inbound")
        })
        .expect("inbound copy must exist");
    let inbound_full_id = inbound_note.id.as_hyphenated().to_string();

    // Reply so there is at least one threaded message.
    khive_reg
        .dispatch(
            "comm.reply",
            serde_json::json!({ "id": outbound_full_id, "content": "a reply" }),
        )
        .await
        .expect("reply succeeds");

    // Query thread via the inbound copy UUID.  Must return all thread messages.
    let thread_via_inbound = khive_reg
        .dispatch("comm.thread", serde_json::json!({ "id": inbound_full_id }))
        .await
        .expect("H1: thread query via inbound UUID must succeed");

    let count_via_inbound = thread_via_inbound
        .get("count")
        .and_then(|v| v.as_u64())
        .expect("count");

    // Query thread via the outbound copy UUID for comparison.
    let thread_via_outbound = khive_reg
        .dispatch("comm.thread", serde_json::json!({ "id": outbound_full_id }))
        .await
        .expect("thread query via outbound UUID must succeed");
    let count_via_outbound = thread_via_outbound
        .get("count")
        .and_then(|v| v.as_u64())
        .expect("count");

    assert_eq!(
        count_via_inbound, count_via_outbound,
        "H1: thread query via inbound UUID must return same count as outbound UUID; \
         via_inbound={count_via_inbound} via_outbound={count_via_outbound}"
    );
    assert!(
        count_via_inbound >= 2,
        "H1: thread must contain at least root + reply; got count={count_via_inbound}"
    );
}

// ── M1 regression: list(kind=message) paginated scan past backlog ─────────────

/// list(kind=message, direction=inbound) must find a matching message even when
/// more than 1000 non-matching outbound messages precede it in the store.
///
/// Before the fix, the handler fetched at most (limit*10).min(1000) rows and
/// applied an in-memory filter — a single matching message buried beyond 1000
/// non-matching rows would be silently missed.
///
/// After the fix, the handler paginates through the store in 200-row chunks until
/// either `limit` filtered matches are collected or the scan ceiling (10000) is
/// reached.
#[tokio::test]
async fn test_list_message_finds_match_beyond_1000_backlog() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");

    let tok = rt.authorize(Namespace::parse("lambda:khive").unwrap());

    // Write the inbound target FIRST so it is stored with the earliest created_at.
    // Notes are returned newest-first by the DB; if the target were written last it
    // would land at position 0 and be visible without paginating past the backlog —
    // defeating the regression this test guards against.
    let target = rt
        .create_note(
            &tok,
            "message",
            None,
            "the matching inbound message",
            None,
            Some(serde_json::json!({
                "from": "lambda:other",
                "to": "lambda:khive",
                "direction": "inbound",
                "read": false,
                "sent_at": chrono::Utc::now().to_rfc3339(),
            })),
            vec![],
        )
        .await
        .expect("create inbound target");
    let target_id = target.id.to_string();

    // Write 1001 outbound noise rows AFTER the target so they sort before it
    // (newest-first) and bury the target beyond the old 1000-row prefetch cap.
    for i in 0..1001u32 {
        rt.create_note(
            &tok,
            "message",
            None,
            &format!("outbound noise {i}"),
            None,
            Some(serde_json::json!({
                "from": "lambda:khive",
                "to": "lambda:other",
                "direction": "outbound",
                "read": false,
                "sent_at": chrono::Utc::now().to_rfc3339(),
            })),
            vec![],
        )
        .await
        .expect("create outbound note");
    }

    // Build a kg-scoped registry in the same namespace for list().
    let mut list_builder = VerbRegistryBuilder::new();
    list_builder.register(khive_pack_kg::KgPack::new(rt.clone()));
    list_builder.register(CommPack::new(rt.clone()));
    list_builder.with_default_namespace("lambda:khive");
    let list_registry = list_builder.build().expect("list registry");

    let result = list_registry
        .dispatch(
            "list",
            serde_json::json!({ "kind": "message", "direction": "inbound", "limit": 1 }),
        )
        .await
        .expect("list(direction=inbound) succeeds");

    let items = result.as_array().expect("list returns array");
    assert_eq!(
        items.len(),
        1,
        "M1 regression: list(kind=message, direction=inbound) must find the 1 matching \
         message buried after 1001 outbound messages; got {} items",
        items.len()
    );
    let dir = items[0]
        .get("properties")
        .and_then(|p| p.get("direction"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert_eq!(
        dir, "inbound",
        "M1: returned message must have direction=inbound; got {dir}"
    );
    // Confirm the returned item is the exact target we wrote (not some other inbound row).
    let returned_id = items[0].get("id").and_then(|v| v.as_str()).unwrap_or("");
    assert_eq!(
        returned_id, target_id,
        "M1: returned item id={returned_id} must match the target id={target_id}"
    );
}
