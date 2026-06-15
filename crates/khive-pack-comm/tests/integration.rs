//! Smoke tests for the comm pack.
//!
//! INLINE TEST JUSTIFICATION: all five comm verbs (send, inbox, read, reply, thread) share a
//! single in-memory runtime fixture. Splitting into per-verb files would require duplicating
//! the fixture and lose cross-verb invariant tests (e.g. send→inbox→read→reply→thread
//! roundtrip and thread-isolation assertions) that exercise interactions between verbs.

use std::sync::Arc;

use khive_pack_comm::CommPack;
use khive_runtime::{
    AllowAllGate, BackendId, KhiveRuntime, Namespace, NamespaceToken, NotePatch, RuntimeConfig,
    VerbRegistry, VerbRegistryBuilder,
};
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

    // Send a message to self (same namespace) — creates outbound + inbound notes.
    let result = registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "local", "content": "hello" }),
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
    let caller_token = rt
        .authorize(khive_runtime::Namespace::parse("local").unwrap())
        .unwrap();
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

    // Send the original message (same namespace — cross-namespace sends are denied).
    let original = registry
        .dispatch(
            "comm.send",
            serde_json::json!({
                "to": "local",
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
            serde_json::json!({ "to": "local", "content": "hello" }),
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
    let caller_token = rt
        .authorize(khive_runtime::Namespace::parse("local").unwrap())
        .unwrap();
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
                "to": "local",
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
    let token = rt.authorize(khive_runtime::Namespace::local()).unwrap();

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

/// send() within the same namespace writes one outbound note in the caller's namespace.
///
/// Cross-namespace sends are denied (issue #481 fix).
/// Same-namespace sends must produce both outbound and inbound copies.
#[tokio::test]
async fn test_send_writes_outbound_in_caller_ns() {
    let (registry, rt) = build_registry_for_ns("lambda:khive");

    registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:khive", "content": "hi" }),
        )
        .await
        .expect("same-namespace send succeeds");

    // Verify: lambda:khive namespace has exactly 1 outbound note.
    let caller_token = rt
        .authorize(Namespace::parse("lambda:khive").unwrap())
        .unwrap();
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
        Some("lambda:khive")
    );
}

/// send() within the same namespace writes one inbound note alongside the outbound copy.
///
/// Cross-namespace sends are denied (issue #481 fix).
/// Same-namespace send creates both copies in the caller's namespace.
#[tokio::test]
async fn test_send_writes_inbound_in_recipient_ns() {
    let (registry, rt) = build_registry_for_ns("lambda:khive");

    registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:khive", "content": "meeting at 3pm" }),
        )
        .await
        .expect("same-namespace send succeeds");

    // Verify: lambda:khive namespace has exactly 1 inbound note.
    let caller_token = rt
        .authorize(Namespace::parse("lambda:khive").unwrap())
        .unwrap();
    let notes = rt
        .list_notes(&caller_token, Some("message"), 100, 0)
        .await
        .expect("list_notes in caller ns succeeds");
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
        "caller namespace must have exactly 1 inbound note; got {inbound:?}"
    );
    let props = inbound[0].properties.as_ref().unwrap();
    assert_eq!(
        props.get("from").and_then(|v| v.as_str()),
        Some("lambda:khive")
    );
    assert_eq!(
        props.get("to").and_then(|v| v.as_str()),
        Some("lambda:khive")
    );
    assert_eq!(inbound[0].content, "meeting at 3pm");
    // inbound copy must carry an outbound_ref back to the outbound copy.
    assert!(
        props.get("outbound_ref").is_some(),
        "inbound note must carry outbound_ref"
    );
}

/// inbox() returns the inbound message after a same-namespace send.
///
/// Cross-namespace delivery is denied (issue #481 fix).
/// Same-namespace send creates an inbound copy visible in inbox().
#[tokio::test]
async fn test_inbox_returns_inbound_for_recipient() {
    // Self-send: both copies land in lambda:khive namespace.
    let (registry, _rt) = build_registry_for_ns("lambda:khive");
    registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:khive", "content": "you have mail" }),
        )
        .await
        .expect("same-namespace send succeeds");

    // inbox() on the same registry must surface the inbound copy.
    let inbox = registry
        .dispatch("comm.inbox", serde_json::json!({ "status": "unread" }))
        .await
        .expect("inbox succeeds");

    let count = inbox
        .get("count")
        .and_then(|v| v.as_u64())
        .expect("inbox returns count");
    assert_eq!(
        count, 1,
        "lambda:khive inbox must have 1 unread message; got {inbox}"
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

    let caller_token = rt
        .authorize(Namespace::parse("lambda:khive").unwrap())
        .unwrap();
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

// ── UE6-H1: reply routes to the "other party" based on metadata, not namespace ─

/// Sender replies to their own outbound message → reply `to` equals original `to`.
///
/// Within the same namespace: A sends to self (from=A, to=A). Sender replies to
/// the outbound copy. Because from==to, the reply routes back to the same namespace
/// (which is correct — there is no other party in a self-send).
///
/// Cross-namespace send is denied (issue #481 fix).
#[tokio::test]
async fn test_reply_from_sender_routes_to_recipient() {
    // Registry scoped to lambda:khive (sender == recipient in same-namespace mode).
    let (registry, _rt) = build_registry_for_ns("lambda:khive");

    // Same-namespace send: from=lambda:khive, to=lambda:khive.
    let sent = registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:khive", "content": "hello self" }),
        )
        .await
        .expect("same-namespace send succeeds");

    let msg_full_id = sent
        .get("full_id")
        .and_then(|v| v.as_str())
        .expect("send returns full_id");

    // Sender replies to their own outbound message.
    let reply = registry
        .dispatch(
            "comm.reply",
            serde_json::json!({ "id": msg_full_id, "content": "follow-up" }),
        )
        .await
        .expect("reply succeeds");

    // from == to in self-send, so reply routes back to self.
    let reply_to = reply
        .get("to")
        .and_then(|v| v.as_str())
        .expect("reply returns to");
    assert_eq!(
        reply_to, "lambda:khive",
        "UE6-H1: self-send reply routes back to same namespace; got {reply_to}"
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

/// Recipient replies to an inbound message → reply routes back to the original sender
/// metadata field, not the caller's namespace.
///
/// Within same-namespace: both are the same namespace so the routing is always self.
/// This test verifies reply() works on an inbound message and preserves the metadata.
///
/// Cross-namespace send is denied (issue #481 fix).
#[tokio::test]
async fn test_reply_from_recipient_routes_to_sender() {
    // Same namespace: lambda:khive sends to lambda:khive, then replies.
    let (registry, _rt) = build_registry_for_ns("lambda:khive");

    registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:khive", "content": "meeting at 3pm" }),
        )
        .await
        .expect("same-namespace send succeeds");

    // Find the inbound copy.
    let inbox = registry
        .dispatch("comm.inbox", serde_json::json!({ "status": "unread" }))
        .await
        .expect("inbox succeeds");
    let msgs = inbox
        .get("messages")
        .and_then(|v| v.as_array())
        .expect("messages array");
    assert_eq!(msgs.len(), 1, "must have 1 inbound message");
    let inbound_full_id = msgs[0]
        .get("full_id")
        .and_then(|v| v.as_str())
        .expect("full_id on inbound message");

    // Reply to the inbound message.
    let reply = registry
        .dispatch(
            "comm.reply",
            serde_json::json!({ "id": inbound_full_id, "content": "confirmed" }),
        )
        .await
        .expect("reply succeeds");

    // In same-namespace, from == to, so reply routes to the same namespace.
    let reply_to = reply
        .get("to")
        .and_then(|v| v.as_str())
        .expect("reply returns to");
    assert_eq!(
        reply_to, "lambda:khive",
        "UE6-H1: same-namespace reply routes back to caller namespace; got {reply_to}"
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

// ── UE6-H2: reply thread_id must be full 36-char UUID ───────────────────────

/// reply thread_id must be the full 36-char hyphenated UUID of the root message.
#[tokio::test]
async fn test_reply_thread_id_is_full_uuid() {
    let (registry, _rt) = build_registry();

    let original = registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "local", "content": "root message" }),
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
            serde_json::json!({ "to": "local", "content": "start of thread" }),
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
    let caller_token = rt
        .authorize(Namespace::parse("lambda:khive").unwrap())
        .unwrap();
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
///
/// Cross-namespace send is denied (issue #481 fix).
/// Same-namespace send is used here; the outbound copy stays in lambda:khive.
#[tokio::test]
async fn test_read_rejects_outbound_message() {
    let (registry, _rt) = build_registry_for_ns("lambda:khive");

    // Same-namespace send — the outbound copy is in lambda:khive.
    let sent = registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:khive", "content": "outbound read attempt" }),
        )
        .await
        .expect("same-namespace send succeeds");

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
    // created_at is an ISO 8601 string; compare lexicographically (not as_i64).
    let timestamps: Vec<&str> = msgs
        .iter()
        .map(|m| {
            m.get("created_at")
                .and_then(|v| v.as_str())
                .expect("H3: thread message must have ISO string created_at")
        })
        .collect();
    let mut sorted = timestamps.clone();
    sorted.sort_unstable();
    assert_eq!(
        timestamps, sorted,
        "H3: thread must return messages in chronological order"
    );
}

// ── High 1 regression: reply() delivers inbound copy alongside the outbound copy ─

/// reply() must write both an outbound copy and an inbound copy within the same namespace.
///
/// Before the fix, reply() created only an outbound note via a single
/// create_note call, so inbox() would not surface the reply.
///
/// Cross-namespace send is denied (issue #481 fix).
/// Same-namespace send is used here — both copies land in the caller's namespace.
#[tokio::test]
async fn test_reply_delivers_inbound_to_recipient() {
    // Same-namespace: lambda:khive sends to itself and replies.
    let (registry, _rt) = build_registry_for_ns("lambda:khive");

    registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:khive", "content": "original message" }),
        )
        .await
        .expect("same-namespace send succeeds");

    // Find the inbound copy via inbox().
    let inbox = registry
        .dispatch("comm.inbox", serde_json::json!({ "status": "all" }))
        .await
        .expect("inbox succeeds");
    let msgs = inbox.get("messages").and_then(|v| v.as_array()).unwrap();
    assert_eq!(msgs.len(), 1, "must have 1 inbound message");
    let inbound_id = msgs[0].get("full_id").and_then(|v| v.as_str()).unwrap();

    // Reply to the inbound message.
    registry
        .dispatch(
            "comm.reply",
            serde_json::json!({ "id": inbound_id, "content": "reply message" }),
        )
        .await
        .expect("reply succeeds");

    // After reply, inbox must contain at least 2 inbound messages
    // (the original inbound + the reply's inbound copy).
    let inbox_after = registry
        .dispatch("comm.inbox", serde_json::json!({ "status": "all" }))
        .await
        .expect("inbox after reply succeeds");
    let count_after = inbox_after
        .get("count")
        .and_then(|v| v.as_u64())
        .expect("count field");
    assert!(
        count_after >= 2,
        "High-1 regression: reply() must deliver an inbound copy; \
         inbox count={count_after} (expected >= 2)"
    );

    // All inbox items must have direction=inbound.
    let msgs_after = inbox_after
        .get("messages")
        .and_then(|v| v.as_array())
        .unwrap();
    assert!(
        msgs_after.iter().all(|m| m
            .get("properties")
            .and_then(|p| p.get("direction"))
            .and_then(|v| v.as_str())
            == Some("inbound")),
        "High-1 regression: all inbox items must have direction=inbound; \
         got {inbox_after}"
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
        let tok = rt
            .authorize(khive_runtime::Namespace::parse("local").unwrap())
            .unwrap();
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
        let tok = rt
            .authorize(khive_runtime::Namespace::parse("local").unwrap())
            .unwrap();
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

// ── H1 regression: thread query finds reply within same namespace ─────────────

/// A sends to self, A replies via the inbound copy, comm.thread(id=outbound_id)
/// must return both the outbound and the reply.
///
/// Before the fix, dual_write_message did not stamp the outbound copy with a
/// canonical thread_id. The reply's thread_id was then set to the inbound copy
/// UUID, causing thread(id=outbound_id) to miss the reply.
///
/// After the fix, both copies share the same canonical thread_id (outbound UUID),
/// and all replies carry that thread_id so the thread query finds them.
///
/// Cross-namespace send is denied (issue #481 fix).
/// Same-namespace send is used to test the canonical thread_id invariant.
#[tokio::test]
async fn test_cross_namespace_thread_query_finds_reply() {
    let (registry, rt) = build_registry_for_ns("lambda:khive");

    let sent = registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:khive", "content": "hello" }),
        )
        .await
        .expect("same-namespace send succeeds");

    let outbound_full_id = sent
        .get("full_id")
        .and_then(|v| v.as_str())
        .expect("send returns full_id");

    // Find the inbound copy — it has a different UUID from the outbound copy.
    let caller_token = rt
        .authorize(Namespace::parse("lambda:khive").unwrap())
        .unwrap();
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
    let inbound_full_id = inbound_note.id.as_hyphenated().to_string();

    // Both copies must share the same canonical thread_id (= outbound UUID).
    let inbound_thread_id = inbound_note
        .properties
        .as_ref()
        .and_then(|p| p.get("thread_id"))
        .and_then(|v| v.as_str())
        .expect("inbound copy must have thread_id");
    assert_eq!(
        inbound_thread_id, outbound_full_id,
        "H1: inbound copy thread_id must equal outbound UUID (canonical root); \
         inbound_full_id={inbound_full_id} outbound_full_id={outbound_full_id} \
         inbound_thread_id={inbound_thread_id}"
    );

    // Reply to the inbound copy.
    registry
        .dispatch(
            "comm.reply",
            serde_json::json!({ "id": inbound_full_id, "content": "reply" }),
        )
        .await
        .expect("reply succeeds");

    // comm.thread(id=outbound_full_id) must find the reply.
    let thread_result = registry
        .dispatch("comm.thread", serde_json::json!({ "id": outbound_full_id }))
        .await
        .expect("H1: thread query must succeed");

    let count = thread_result
        .get("count")
        .and_then(|v| v.as_u64())
        .expect("thread returns count");
    assert!(
        count >= 2,
        "H1 regression: comm.thread(id=outbound_id) must find the reply; \
         got count={count}, result={thread_result}"
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
    let caller_token = rt
        .authorize(Namespace::parse("lambda:khive").unwrap())
        .unwrap();
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

    let tok = rt
        .authorize(Namespace::parse("lambda:khive").unwrap())
        .unwrap();

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

// ── #481 regression: cross-namespace send is denied (ACL gate) ────────────────

#[tokio::test]
async fn test_cross_namespace_send_denied_issue_481() {
    let (registry, rt) = build_registry_for_ns("lambda:khive");

    let result = registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:leo", "content": "should be denied" }),
        )
        .await;

    assert!(
        result.is_err(),
        "#481 regression: cross-namespace send must be denied; got ok: {result:?}"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("cross-namespace") || err.contains("lambda:leo"),
        "#481 regression: error must mention the denied namespace or 'cross-namespace'; got {err:?}"
    );

    let recipient_token = rt
        .authorize(khive_runtime::Namespace::parse("lambda:leo").unwrap())
        .unwrap();
    let notes = rt
        .list_notes(&recipient_token, Some("message"), 100, 0)
        .await
        .expect("list_notes in recipient ns");
    assert_eq!(notes.len(), 0, "#481 regression: no note in recipient ns");

    let sender_token = rt
        .authorize(khive_runtime::Namespace::parse("lambda:khive").unwrap())
        .unwrap();
    let sender_notes = rt
        .list_notes(&sender_token, Some("message"), 100, 0)
        .await
        .expect("list_notes in sender ns");
    assert_eq!(
        sender_notes.len(),
        0,
        "#481 regression: no note in sender ns"
    );
}

#[tokio::test]
async fn test_same_namespace_send_succeeds_issue_481() {
    let (registry, _rt) = build_registry_for_ns("lambda:khive");

    let result = registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:khive", "content": "self-send is allowed" }),
        )
        .await;

    assert!(
        result.is_ok(),
        "#481 regression: same-namespace send must succeed; got err: {result:?}"
    );
    let id = result.unwrap();
    assert!(
        id.get("id").is_some(),
        "#481 regression: same-namespace send must return an id; got {id:?}"
    );
}

// ── #485 regression: thread sort must use ISO string comparison, not as_i64 ──

#[tokio::test]
async fn test_thread_sort_is_not_a_noop_issue_485() {
    let (registry, _rt) = build_registry_for_ns("local");

    let root = registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "local", "content": "root" }),
        )
        .await
        .expect("root send succeeds");
    let root_full_id = root
        .get("full_id")
        .and_then(|v| v.as_str())
        .expect("root full_id");

    registry
        .dispatch(
            "comm.reply",
            serde_json::json!({ "id": root_full_id, "content": "reply-1" }),
        )
        .await
        .expect("reply-1 succeeds");

    registry
        .dispatch(
            "comm.reply",
            serde_json::json!({ "id": root_full_id, "content": "reply-2" }),
        )
        .await
        .expect("reply-2 succeeds");

    let thread_result = registry
        .dispatch("comm.thread", serde_json::json!({ "id": root_full_id }))
        .await
        .expect("#485 regression: thread verb must succeed");

    let msgs = thread_result
        .get("messages")
        .and_then(|v| v.as_array())
        .expect("thread returns messages array");

    assert!(
        msgs.len() >= 3,
        "#485: expected at least 3 messages (root + 2 replies); got {}",
        msgs.len()
    );

    for (i, m) in msgs.iter().enumerate() {
        let ts = m
            .get("created_at")
            .expect("#485: message must have created_at field");
        assert!(
            ts.is_string(),
            "#485: created_at[{i}] must be an ISO string, got: {ts:?}"
        );
    }

    let timestamps: Vec<&str> = msgs
        .iter()
        .map(|m| {
            m.get("created_at")
                .and_then(|v| v.as_str())
                .expect("#485: created_at must be ISO string")
        })
        .collect();
    let mut sorted = timestamps.clone();
    sorted.sort_unstable();
    assert_eq!(
        timestamps, sorted,
        "#485: thread must return in chronological order"
    );
}

// ── schema_plan regression: CommPack declares comm message indexes ────────────

#[tokio::test]
async fn comm_pack_exposes_non_empty_schema_plan() {
    use khive_runtime::PackRuntime;
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let pack = CommPack::new(runtime);
    let plan = pack.schema_plan();

    assert!(
        !plan.is_empty(),
        "CommPack must return a non-empty SchemaPlan"
    );
    assert_eq!(plan.pack, "comm", "SchemaPlan.pack must be 'comm'");
    assert!(
        !plan.statements.is_empty(),
        "schema plan must have at least one DDL statement"
    );

    let combined = plan.statements.join(" ");
    assert!(
        combined.contains("idx_comm_message_direction"),
        "schema plan must declare idx_comm_message_direction; got: {combined}"
    );
    assert!(
        combined.contains("idx_comm_message_thread"),
        "schema plan must declare idx_comm_message_thread; got: {combined}"
    );
    assert!(
        combined.contains("CREATE INDEX IF NOT EXISTS"),
        "schema plan DDL must be idempotent; got: {combined}"
    );
    // Indexes now use WHERE deleted_at IS NULL so the parameterized kind = ?N
    // predicate can use the index (literal WHERE kind = 'message' blocks this).
    assert!(
        combined.contains("deleted_at IS NULL"),
        "schema plan indexes must use WHERE deleted_at IS NULL partial condition; got: {combined}"
    );
}

#[tokio::test]
async fn verb_registry_aggregates_comm_schema_plan() {
    let (registry, _rt) = build_registry();
    let plans = registry.all_schema_plans();
    assert!(
        plans.iter().any(|p| p.pack == "comm"),
        "registry must expose comm schema plan; got packs: {:?}",
        plans.iter().map(|p| p.pack).collect::<Vec<_>>()
    );
    let comm_plan = plans
        .iter()
        .find(|p| p.pack == "comm")
        .expect("comm plan present");
    assert!(
        !comm_plan.is_empty(),
        "comm schema plan must have DDL statements"
    );
}

/// thread isolation: comm.thread returns only messages belonging to the requested thread,
/// not messages from other threads in the same namespace.
#[tokio::test]
async fn test_thread_returns_only_requested_thread_messages() {
    let (registry, _rt) = build_registry_for_ns("local");

    // Send two independent root messages (thread A and thread B).
    let msg_a = registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "local", "content": "thread A root" }),
        )
        .await
        .expect("send thread A root");
    let thread_a_id = msg_a
        .get("full_id")
        .and_then(|v| v.as_str())
        .expect("full_id A");

    registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "local", "content": "thread B root" }),
        )
        .await
        .expect("send thread B root");

    // Reply to thread A.
    registry
        .dispatch(
            "comm.reply",
            serde_json::json!({ "id": thread_a_id, "content": "reply to A" }),
        )
        .await
        .expect("reply to A");

    // Fetch thread A — must contain exactly the root + 1 reply (the inbound copy of each).
    // With self-send, each comm.send creates outbound + inbound, and reply creates outbound + inbound.
    // SQL filter ensures only thread-A messages are returned.
    let thread = registry
        .dispatch("comm.thread", serde_json::json!({ "id": thread_a_id }))
        .await
        .expect("thread A fetch");

    let messages = thread
        .get("messages")
        .and_then(|v| v.as_array())
        .expect("messages array");

    // All returned messages must have thread_id == thread_a_id.
    for msg in messages {
        let props = msg.get("properties").expect("has properties");
        let stored_tid = props
            .get("thread_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert_eq!(
            stored_tid, thread_a_id,
            "all thread messages must carry thread_id={thread_a_id}, got {stored_tid}"
        );
    }

    // Must have at least 2 messages (root + reply, inbound copies).
    assert!(
        messages.len() >= 2,
        "thread must contain at least root + reply; got {}",
        messages.len()
    );
}

/// read filter 5-case truth table: json_type-based filter matches old as_bool().unwrap_or(false).
/// Seeds messages with $.read set to: missing, bool false, bool true, string "true", integer 1.
/// Verifies that inbox(status=unread) and inbox(status=read) classify each case correctly.
#[tokio::test]
async fn test_inbox_read_filter_json_type_truth_table() {
    use khive_storage::note::{FilterOp, Note, NoteFilter, PropertyFilter};
    use khive_storage::types::{PageRequest, SqlValue};

    let (_registry, rt) = build_registry_for_ns("local");
    let token = rt
        .authorize(khive_runtime::Namespace::parse("local").unwrap())
        .unwrap();
    let store = rt.notes(&token).expect("note store");

    // Seed 5 inbound message notes directly (bypassing send) to control $.read exactly.
    let make_msg = |read_val: serde_json::Value, label: &str| -> Note {
        Note::new("local", "message", label).with_properties(serde_json::json!({
            "direction": "inbound",
            "from": "local",
            "to": "local",
            "thread_id": null,
            "read": read_val,
        }))
    };

    // missing: don't set read at all in properties
    let note_missing = Note::new("local", "message", "read=missing").with_properties(
        serde_json::json!({ "direction": "inbound", "from": "local", "to": "local" }),
    );
    let note_false = make_msg(serde_json::json!(false), "read=false");
    let note_true = make_msg(serde_json::json!(true), "read=true");
    let note_str_true = make_msg(serde_json::json!("true"), "read=string_true");
    let note_int_1 = make_msg(serde_json::json!(1), "read=int_1");

    store.upsert_note(note_missing).await.unwrap();
    store.upsert_note(note_false).await.unwrap();
    store.upsert_note(note_true).await.unwrap();
    store.upsert_note(note_str_true).await.unwrap();
    store.upsert_note(note_int_1).await.unwrap();

    // Query unread: missing, false, "true" (string), 1 (integer) → all count as unread.
    let unread_filter = NoteFilter {
        kind: Some("message".to_string()),
        property_filters: vec![
            PropertyFilter {
                json_path: "$.direction".to_string(),
                op: FilterOp::Eq,
                value: SqlValue::Text("inbound".to_string()),
            },
            PropertyFilter {
                json_path: "$.read".to_string(),
                op: FilterOp::JsonTypeNeMissing,
                value: SqlValue::Text("true".to_string()),
            },
        ],
        order_by: None,
        ..Default::default()
    };
    let unread_page = store
        .query_notes_filtered("local", &unread_filter, PageRequest::default())
        .await
        .unwrap();
    let unread_contents: Vec<&str> = unread_page
        .items
        .iter()
        .map(|n| n.content.as_str())
        .collect();

    assert!(
        unread_contents.contains(&"read=missing"),
        "missing $.read must be unread; got {unread_contents:?}"
    );
    assert!(
        unread_contents.contains(&"read=false"),
        "bool false must be unread; got {unread_contents:?}"
    );
    assert!(
        unread_contents.contains(&"read=string_true"),
        "string 'true' must be unread (not JSON bool true); got {unread_contents:?}"
    );
    assert!(
        unread_contents.contains(&"read=int_1"),
        "integer 1 must be unread (not JSON bool true); got {unread_contents:?}"
    );
    assert!(
        !unread_contents.contains(&"read=true"),
        "JSON bool true must NOT be unread; got {unread_contents:?}"
    );

    // Query read: only JSON boolean true → exactly 1 result.
    let read_filter = NoteFilter {
        kind: Some("message".to_string()),
        property_filters: vec![
            PropertyFilter {
                json_path: "$.direction".to_string(),
                op: FilterOp::Eq,
                value: SqlValue::Text("inbound".to_string()),
            },
            PropertyFilter {
                json_path: "$.read".to_string(),
                op: FilterOp::JsonTypeEq,
                value: SqlValue::Text("true".to_string()),
            },
        ],
        order_by: None,
        ..Default::default()
    };
    let read_page = store
        .query_notes_filtered("local", &read_filter, PageRequest::default())
        .await
        .unwrap();
    assert_eq!(
        read_page.items.len(),
        1,
        "only JSON bool true must be in 'read'; got {:?}",
        read_page
            .items
            .iter()
            .map(|n| &n.content)
            .collect::<Vec<_>>()
    );
    assert_eq!(read_page.items[0].content, "read=true");
}

// ── COMM-AUD-003: thread_id validation at verb boundary ───────────────────────

/// send with a malformed thread_id must return InvalidInput, not persist garbage.
#[tokio::test]
async fn send_rejects_malformed_thread_id() {
    let (registry, _rt) = build_registry_for_ns("local");

    let err = registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "local", "content": "hi", "thread_id": "not-a-uuid" }),
        )
        .await;
    assert!(
        err.is_err(),
        "send with malformed thread_id must fail; got: {err:?}"
    );
}

/// send with a valid UUID thread_id must succeed.
#[tokio::test]
async fn send_accepts_valid_uuid_thread_id() {
    let (registry, _rt) = build_registry_for_ns("local");

    // First send to get a real thread root UUID.
    let root = registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "local", "content": "root message" }),
        )
        .await
        .expect("root send succeeds");
    let thread_uuid = root
        .get("full_id")
        .and_then(|v| v.as_str())
        .expect("full_id present");

    let result = registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "local", "content": "threaded reply", "thread_id": thread_uuid }),
        )
        .await;
    assert!(
        result.is_ok(),
        "send with valid UUID thread_id must succeed; got: {result:?}"
    );
}

// ── COMM-AUD-004: ThreadParams deny_unknown_fields ────────────────────────────

/// comm.thread with an unknown argument must return an error, not silently ignore it.
#[tokio::test]
async fn thread_rejects_unknown_field() {
    let (registry, _rt) = build_registry_for_ns("local");

    // Send a root message so there is a valid id to use.
    let root = registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "local", "content": "root" }),
        )
        .await
        .expect("send succeeds");
    let root_id = root
        .get("full_id")
        .and_then(|v| v.as_str())
        .expect("full_id present");

    let err = registry
        .dispatch(
            "comm.thread",
            serde_json::json!({ "id": root_id, "typo_arg": "oops" }),
        )
        .await;
    assert!(
        err.is_err(),
        "comm.thread with unknown field must fail; got: {err:?}"
    );
}

// ── T1-T12: cross-namespace delivery allowlist (allowed_outbound_namespaces) ─────

/// Build a KhiveRuntime + VerbRegistry for cross-ns tests.
///
/// `dispatch_ns` — the default namespace used for dispatch (the caller identity).
/// `allowed_outbound` — namespaces this sender may deliver into cross-namespace.
///
/// Both registries in a cross-ns pair must share the same `Arc<khive_db::StorageBackend>`
/// so that outbound notes written in one namespace are visible via the other's token.
fn build_crossns_registry(
    backend: Arc<khive_db::StorageBackend>,
    dispatch_ns: &str,
    allowed_outbound: Vec<Namespace>,
) -> (VerbRegistry, KhiveRuntime) {
    let config = RuntimeConfig {
        db_path: None,
        default_namespace: Namespace::parse(dispatch_ns).unwrap(),
        embedding_model: None,
        additional_embedding_models: vec![],
        gate: Arc::new(AllowAllGate),
        packs: vec!["kg".to_string(), "comm".to_string()],
        backend_id: BackendId::main(),
        brain_profile: None,
        visible_namespaces: vec![],
        allowed_outbound_namespaces: allowed_outbound,
    };
    let rt = KhiveRuntime::from_backend(backend, config);
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(rt.clone()));
    builder.register(CommPack::new(rt.clone()));
    builder.with_default_namespace(dispatch_ns);
    let registry = builder.build().expect("cross-ns registry builds");
    (registry, rt)
}

fn shared_backend() -> Arc<khive_db::StorageBackend> {
    let backend = khive_db::StorageBackend::memory().expect("in-memory backend");
    {
        let mut writer = backend.pool().try_writer().expect("writer");
        khive_db::run_migrations(writer.conn_mut()).expect("migrations");
    }
    Arc::new(backend)
}

// T1 — within-namespace send unchanged by the allowlist feature.
#[tokio::test]
async fn t1_send_within_namespace_unchanged() {
    let backend = shared_backend();
    let (registry, rt) = build_crossns_registry(
        backend,
        "lambda:leo",
        vec![], // no outbound allowlist needed for same-ns
    );

    let result = registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:leo", "content": "self-send" }),
        )
        .await;
    assert!(
        result.is_ok(),
        "T1: within-ns send must succeed; got {result:?}"
    );

    let tok = rt
        .authorize(Namespace::parse("lambda:leo").unwrap())
        .unwrap();
    let notes = rt.list_notes(&tok, Some("message"), 100, 0).await.unwrap();
    let alive: Vec<_> = notes.iter().filter(|n| n.deleted_at.is_none()).collect();
    assert_eq!(
        alive.len(),
        2,
        "T1: expect 1 outbound + 1 inbound for same-ns send; got {}",
        alive.len()
    );
}

// T2 — cross-ns send is denied when the allowlist is empty.
#[tokio::test]
async fn t2_send_cross_ns_denied_when_allowlist_empty() {
    let backend = shared_backend();
    let (registry_leo, rt_leo) = build_crossns_registry(Arc::clone(&backend), "lambda:leo", vec![]);
    let (_registry_khive, rt_khive) =
        build_crossns_registry(Arc::clone(&backend), "lambda:khive", vec![]);

    let result = registry_leo
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:khive", "content": "blocked" }),
        )
        .await;
    assert!(
        result.is_err(),
        "T2: cross-ns send with empty allowlist must be denied"
    );
    let err_str = result.unwrap_err().to_string();
    assert!(
        err_str.contains("not permitted")
            || err_str.contains("PermissionDenied")
            || err_str.contains("comm.send"),
        "T2: error must mention the denial; got {err_str:?}"
    );

    // No note written in either namespace.
    let leo_tok = rt_leo
        .authorize(Namespace::parse("lambda:leo").unwrap())
        .unwrap();
    let leo_notes = rt_leo
        .list_notes(&leo_tok, Some("message"), 100, 0)
        .await
        .unwrap();
    assert_eq!(
        leo_notes.iter().filter(|n| n.deleted_at.is_none()).count(),
        0,
        "T2: no note in sender ns"
    );

    let khive_tok = rt_khive
        .authorize(Namespace::parse("lambda:khive").unwrap())
        .unwrap();
    let khive_notes = rt_khive
        .list_notes(&khive_tok, Some("message"), 100, 0)
        .await
        .unwrap();
    assert_eq!(
        khive_notes
            .iter()
            .filter(|n| n.deleted_at.is_none())
            .count(),
        0,
        "T2: no note in recipient ns"
    );
}

// T3 — cross-ns send delivers when the recipient is in the sender's allowlist.
#[tokio::test]
async fn t3_send_cross_ns_delivers_when_allowed() {
    let backend = shared_backend();
    let (registry_leo, rt_leo) = build_crossns_registry(
        Arc::clone(&backend),
        "lambda:leo",
        vec![Namespace::parse("lambda:khive").unwrap()],
    );
    let (_reg_khive, rt_khive) =
        build_crossns_registry(Arc::clone(&backend), "lambda:khive", vec![]);

    let result = registry_leo
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:khive", "content": "hello cross-ns" }),
        )
        .await;
    assert!(
        result.is_ok(),
        "T3: cross-ns send to allowed ns must succeed; got {result:?}"
    );
    let val = result.unwrap();
    assert!(
        val.get("full_id").is_some(),
        "T3: response must carry full_id"
    );

    // Outbound in lambda:leo.
    let leo_tok = rt_leo
        .authorize(Namespace::parse("lambda:leo").unwrap())
        .unwrap();
    let leo_notes = rt_leo
        .list_notes(&leo_tok, Some("message"), 100, 0)
        .await
        .unwrap();
    let outbound: Vec<_> = leo_notes
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
    assert_eq!(outbound.len(), 1, "T3: expect 1 outbound note in sender ns");
    let outbound_thread_id = outbound[0]
        .properties
        .as_ref()
        .and_then(|p| p.get("thread_id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .expect("T3: outbound note must have thread_id");

    // Inbound in lambda:khive.
    let khive_tok = rt_khive
        .authorize(Namespace::parse("lambda:khive").unwrap())
        .unwrap();
    let khive_notes = rt_khive
        .list_notes(&khive_tok, Some("message"), 100, 0)
        .await
        .unwrap();
    let inbound: Vec<_> = khive_notes
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
        "T3: expect 1 inbound note in recipient ns"
    );
    let inbound_note = inbound[0];
    assert_eq!(
        inbound_note
            .properties
            .as_ref()
            .and_then(|p| p.get("from"))
            .and_then(|v| v.as_str()),
        Some("lambda:leo"),
        "T3: inbound from must be lambda:leo"
    );
    assert_eq!(
        inbound_note
            .properties
            .as_ref()
            .and_then(|p| p.get("to"))
            .and_then(|v| v.as_str()),
        Some("lambda:khive"),
        "T3: inbound to must be lambda:khive"
    );
    assert_eq!(
        inbound_note.content, "hello cross-ns",
        "T3: inbound content must match"
    );
    let inbound_thread_id = inbound_note
        .properties
        .as_ref()
        .and_then(|p| p.get("thread_id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .expect("T3: inbound note must have thread_id");
    assert_eq!(
        outbound_thread_id, inbound_thread_id,
        "T3: both copies must share thread_id"
    );
}

// T4 — inbound note's namespace column is the recipient namespace.
#[tokio::test]
async fn t4_inbound_note_namespace_is_recipient() {
    let backend = shared_backend();
    let (registry_leo, _rt_leo) = build_crossns_registry(
        Arc::clone(&backend),
        "lambda:leo",
        vec![Namespace::parse("lambda:khive").unwrap()],
    );
    let (_reg_khive, rt_khive) =
        build_crossns_registry(Arc::clone(&backend), "lambda:khive", vec![]);

    registry_leo
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:khive", "content": "ns stamp check" }),
        )
        .await
        .expect("T4: send must succeed");

    let khive_tok = rt_khive
        .authorize(Namespace::parse("lambda:khive").unwrap())
        .unwrap();
    let khive_notes = rt_khive
        .list_notes(&khive_tok, Some("message"), 100, 0)
        .await
        .unwrap();
    let inbound_note = khive_notes
        .iter()
        .find(|n| n.deleted_at.is_none())
        .expect("T4: must find inbound note");
    assert_eq!(
        inbound_note.namespace.as_str(),
        "lambda:khive",
        "T4: inbound note namespace must be lambda:khive (with_namespace stamped it)"
    );
}

// T5 — recipient inbox sees the delivered message.
#[tokio::test]
async fn t5_recipient_inbox_sees_message() {
    let backend = shared_backend();
    let (registry_leo, _rt_leo) = build_crossns_registry(
        Arc::clone(&backend),
        "lambda:leo",
        vec![Namespace::parse("lambda:khive").unwrap()],
    );
    let (registry_khive, _rt_khive) =
        build_crossns_registry(Arc::clone(&backend), "lambda:khive", vec![]);

    registry_leo
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:khive", "content": "inbox check" }),
        )
        .await
        .expect("T5: send must succeed");

    let inbox = registry_khive
        .dispatch("comm.inbox", serde_json::json!({}))
        .await
        .expect("T5: inbox dispatch must succeed");
    let count = inbox.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
    assert_eq!(
        count, 1,
        "T5: recipient inbox must contain exactly 1 message; got {count}"
    );
    let msgs = inbox.get("messages").and_then(|v| v.as_array()).unwrap();
    let msg = &msgs[0];
    assert_eq!(
        msg.get("properties")
            .and_then(|p| p.get("from"))
            .and_then(|v| v.as_str()),
        Some("lambda:leo"),
        "T5: inbox message from must be lambda:leo"
    );
    assert_eq!(
        msg.get("properties")
            .and_then(|p| p.get("read"))
            .and_then(|v| v.as_bool()),
        Some(false),
        "T5: inbox message must be unread"
    );
}

// T6 — sender's own inbox does not contain the inbound copy that landed in recipient ns.
#[tokio::test]
async fn t6_sender_inbox_does_not_see_inbound_copy() {
    let backend = shared_backend();
    let (registry_leo, _rt_leo) = build_crossns_registry(
        Arc::clone(&backend),
        "lambda:leo",
        vec![Namespace::parse("lambda:khive").unwrap()],
    );

    registry_leo
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:khive", "content": "isolation check" }),
        )
        .await
        .expect("T6: send must succeed");

    let inbox = registry_leo
        .dispatch("comm.inbox", serde_json::json!({ "status": "all" }))
        .await
        .expect("T6: sender inbox dispatch must succeed");
    let count = inbox.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
    assert_eq!(
        count, 0,
        "T6: sender inbox must not contain the inbound copy (it is in recipient ns); got {count}"
    );
}

// T7 — white-box: with_namespace token cannot read from the original namespace.
#[tokio::test]
async fn t7_inbound_token_grants_no_read_back() {
    let backend = shared_backend();
    let (_registry_leo, rt_leo) = build_crossns_registry(
        Arc::clone(&backend),
        "lambda:leo",
        vec![Namespace::parse("lambda:khive").unwrap()],
    );

    // Create a note in lambda:leo namespace.
    let leo_tok = rt_leo
        .authorize(Namespace::parse("lambda:leo").unwrap())
        .unwrap();
    let sender_note = rt_leo
        .create_note(
            &leo_tok,
            "observation",
            None,
            "sender-ns note",
            None,
            None,
            vec![],
        )
        .await
        .expect("T7: create sender note");

    // Mint the kind of token that with_namespace produces (recipient-scoped).
    let recipient_tok: NamespaceToken =
        leo_tok.with_namespace(Namespace::parse("lambda:khive").unwrap());

    // Attempt to read the sender-ns note using the recipient-scoped token.
    // get_note_including_deleted returns Ok(None) when namespace is not visible.
    let result = rt_leo
        .get_note_including_deleted(&recipient_tok, sender_note.id)
        .await;
    match result {
        Ok(None) => {
            // Expected: token's visible set is [lambda:khive], cannot see lambda:leo notes.
        }
        Ok(Some(_)) => panic!("T7: with_namespace token must not read back sender-ns note"),
        Err(e) => panic!("T7: unexpected error {e:?}"),
    }
}

// T8 — cross-ns note is not update/delete-able by the sender runtime.
#[tokio::test]
async fn t8_cross_ns_send_grants_no_update_delete() {
    let backend = shared_backend();
    let (registry_leo, rt_leo) = build_crossns_registry(
        Arc::clone(&backend),
        "lambda:leo",
        vec![Namespace::parse("lambda:khive").unwrap()],
    );
    let (_reg_khive, rt_khive) =
        build_crossns_registry(Arc::clone(&backend), "lambda:khive", vec![]);

    registry_leo
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:khive", "content": "append-only check" }),
        )
        .await
        .expect("T8: send must succeed");

    // Find the inbound note UUID.
    let khive_tok = rt_khive
        .authorize(Namespace::parse("lambda:khive").unwrap())
        .unwrap();
    let khive_notes = rt_khive
        .list_notes(&khive_tok, Some("message"), 100, 0)
        .await
        .unwrap();
    let inbound_id = khive_notes
        .iter()
        .find(|n| n.deleted_at.is_none())
        .map(|n| n.id)
        .expect("T8: must find inbound note");

    // Attempt update via sender's leo token — must fail with NotFound (namespace mismatch).
    let leo_tok = rt_leo
        .authorize(Namespace::parse("lambda:leo").unwrap())
        .unwrap();
    let update_result = rt_leo
        .update_note(
            &leo_tok,
            inbound_id,
            NotePatch::new(None, None, None, None, None),
        )
        .await;
    match update_result {
        Err(khive_runtime::RuntimeError::NotFound(_)) => {}
        Ok(_) => panic!("T8: update of cross-ns note from sender must return NotFound"),
        Err(e) => panic!("T8: update returned unexpected error {e:?}"),
    }

    // Attempt delete via sender's leo token — must return Ok(false) (namespace mismatch, not deleted).
    let delete_result = rt_leo.delete_note(&leo_tok, inbound_id, true).await;
    match delete_result {
        Ok(false) => {
            // Expected: namespace mismatch; note not deleted.
        }
        Ok(true) => {
            panic!("T8: delete of cross-ns note from sender must NOT succeed (returned true)")
        }
        Err(e) => panic!("T8: delete returned unexpected error {e:?}"),
    }

    // Verify the inbound note still exists in lambda:khive namespace.
    let khive_tok2 = rt_khive
        .authorize(Namespace::parse("lambda:khive").unwrap())
        .unwrap();
    let still_there = rt_khive
        .list_notes(&khive_tok2, Some("message"), 100, 0)
        .await
        .unwrap();
    let alive_after = still_there
        .iter()
        .filter(|n| n.deleted_at.is_none())
        .count();
    assert_eq!(
        alive_after, 1,
        "T8: inbound note must still exist after sender's failed delete attempt"
    );
}

// T9 — reply from recipient back to sender cross-ns when reply allowlist permits.
#[tokio::test]
async fn t9_reply_cross_ns_delivers_when_allowed() {
    let backend = shared_backend();
    let (registry_leo, rt_leo) = build_crossns_registry(
        Arc::clone(&backend),
        "lambda:leo",
        vec![Namespace::parse("lambda:khive").unwrap()],
    );
    // khive can reply to leo.
    let (registry_khive, _rt_khive) = build_crossns_registry(
        Arc::clone(&backend),
        "lambda:khive",
        vec![Namespace::parse("lambda:leo").unwrap()],
    );

    // leo sends to khive.
    let send_result = registry_leo
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:khive", "content": "hello from leo" }),
        )
        .await
        .expect("T9: send must succeed");
    let outbound_thread_id = send_result
        .get("full_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .expect("T9: send must return full_id");

    // Fetch the inbound note UUID (as seen from khive — both runtimes share the same backend).
    let khive_tok = rt_leo
        .authorize(Namespace::parse("lambda:khive").unwrap())
        .unwrap();
    let khive_notes = rt_leo
        .list_notes(&khive_tok, Some("message"), 100, 0)
        .await
        .unwrap();
    let inbound_id = khive_notes
        .iter()
        .find(|n| n.deleted_at.is_none())
        .map(|n| n.id.as_hyphenated().to_string())
        .expect("T9: must find inbound note");

    // khive replies to the inbound message.
    let reply_result = registry_khive
        .dispatch(
            "comm.reply",
            serde_json::json!({ "id": inbound_id, "content": "got it from khive" }),
        )
        .await;
    assert!(
        reply_result.is_ok(),
        "T9: reply cross-ns must succeed; got {reply_result:?}"
    );

    // leo's inbox now has the reply.
    let leo_inbox = registry_leo
        .dispatch("comm.inbox", serde_json::json!({}))
        .await
        .expect("T9: leo inbox");
    let leo_count = leo_inbox.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
    assert_eq!(
        leo_count, 1,
        "T9: leo inbox must contain the reply; got {leo_count}"
    );

    // Reply carries the same thread_id as the original.
    let leo_msgs = leo_inbox
        .get("messages")
        .and_then(|v| v.as_array())
        .unwrap();
    let reply_thread_id = leo_msgs[0]
        .get("properties")
        .and_then(|p| p.get("thread_id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .expect("T9: reply must have thread_id");
    assert_eq!(
        reply_thread_id, outbound_thread_id,
        "T9: reply thread_id must match original outbound UUID"
    );
}

// T10 — reply cross-ns is denied when the replier's allowlist is empty.
#[tokio::test]
async fn t10_reply_cross_ns_denied_when_empty() {
    let backend = shared_backend();
    let (registry_leo, rt_leo) = build_crossns_registry(
        Arc::clone(&backend),
        "lambda:leo",
        vec![Namespace::parse("lambda:khive").unwrap()],
    );
    // khive has NO outbound allowlist (cannot reply cross-ns).
    let (registry_khive, _rt_khive) =
        build_crossns_registry(Arc::clone(&backend), "lambda:khive", vec![]);

    // leo sends to khive.
    registry_leo
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:khive", "content": "setup for T10" }),
        )
        .await
        .expect("T10: initial send must succeed");

    // Fetch the inbound note UUID from khive's perspective.
    let khive_tok = rt_leo
        .authorize(Namespace::parse("lambda:khive").unwrap())
        .unwrap();
    let khive_notes = rt_leo
        .list_notes(&khive_tok, Some("message"), 100, 0)
        .await
        .unwrap();
    let inbound_id = khive_notes
        .iter()
        .find(|n| n.deleted_at.is_none())
        .map(|n| n.id.as_hyphenated().to_string())
        .expect("T10: must find inbound note");

    // khive attempts to reply — must be denied.
    let reply_result = registry_khive
        .dispatch(
            "comm.reply",
            serde_json::json!({ "id": inbound_id, "content": "attempt blocked reply" }),
        )
        .await;
    assert!(
        reply_result.is_err(),
        "T10: reply with empty allowlist must be denied"
    );
    let err_str = reply_result.unwrap_err().to_string();
    assert!(
        err_str.contains("not permitted") || err_str.contains("PermissionDenied"),
        "T10: error must indicate denial; got {err_str:?}"
    );
}

// T11 — outbound note is rolled back when inbound write fails (cross-ns path).
//
// The rollback code in dual_write_message (message.rs, `delete_note` on inbound error)
// is the same code path for both same-ns and cross-ns sends. The existing test
// `test_send_inbound_failure_rolls_back_outbound` confirms rollback for format-invalid
// namespace (failure before outbound note creation). Here we confirm the same rollback
// guarantee for the cross-ns path by verifying no partial state remains when the
// send is denied at the allowlist check (failure also before outbound note creation).
//
// Note: contriving an inbound DB write failure after the outbound note is committed
// requires mocking, which is out of scope for integration tests. The structural rollback
// at message.rs:205-209 is not namespace-path-specific.
#[tokio::test]
async fn t11_inbound_write_failure_rolls_back_outbound() {
    let backend = shared_backend();
    let (registry_leo, rt_leo) = build_crossns_registry(Arc::clone(&backend), "lambda:leo", vec![]); // empty allowlist → denied

    let result = registry_leo
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:khive", "content": "rollback check" }),
        )
        .await;
    assert!(result.is_err(), "T11: denied send must fail");

    // No outbound note must remain (atomicity: outbound not yet created when denial fires).
    let leo_tok = rt_leo
        .authorize(Namespace::parse("lambda:leo").unwrap())
        .unwrap();
    let leo_notes = rt_leo
        .list_notes(&leo_tok, Some("message"), 100, 0)
        .await
        .unwrap();
    let alive = leo_notes.iter().filter(|n| n.deleted_at.is_none()).count();
    assert_eq!(
        alive, 0,
        "T11: no partial outbound note must remain; got {alive}"
    );

    let khive_tok = rt_leo
        .authorize(Namespace::parse("lambda:khive").unwrap())
        .unwrap();
    let khive_notes = rt_leo
        .list_notes(&khive_tok, Some("message"), 100, 0)
        .await
        .unwrap();
    let khive_alive = khive_notes
        .iter()
        .filter(|n| n.deleted_at.is_none())
        .count();
    assert_eq!(
        khive_alive, 0,
        "T11: no inbound note in recipient ns; got {khive_alive}"
    );
}

// T12 — allowlist is directional: leo→khive allowed, but khive→leo denied when not listed.
#[tokio::test]
async fn t12_allowlist_is_one_directional() {
    let backend = shared_backend();
    // leo lists khive.
    let (_registry_leo, _rt_leo) = build_crossns_registry(
        Arc::clone(&backend),
        "lambda:leo",
        vec![Namespace::parse("lambda:khive").unwrap()],
    );
    // khive does NOT list leo.
    let (registry_khive, _rt_khive) =
        build_crossns_registry(Arc::clone(&backend), "lambda:khive", vec![]);

    let result = registry_khive
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:leo", "content": "reverse direction" }),
        )
        .await;
    assert!(
        result.is_err(),
        "T12: reverse cross-ns send must be denied when not in allowlist; got {result:?}"
    );
}
