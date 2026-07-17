//! Smoke tests for the comm pack.
//!
//! INLINE TEST JUSTIFICATION: all five comm verbs (send, inbox, read, reply, thread) share a
//! single in-memory runtime fixture. Splitting into per-verb files would require duplicating
//! the fixture and lose cross-verb invariant tests (e.g. send→inbox→read→reply→thread
//! roundtrip and thread-isolation assertions) that exercise interactions between verbs.

use std::sync::Arc;

use khive_pack_comm::CommPack;
use khive_runtime::{
    AllowAllGate, BackendId, KhiveRuntime, Namespace, NamespaceToken, RuntimeConfig, VerbRegistry,
    VerbRegistryBuilder,
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
fn comm_pack_declares_nine_handlers() {
    assert_eq!(
        CommPack::HANDLERS.len(),
        11,
        "comm pack must declare 11 handlers: send, inbox, read, reply, thread, ingest, \
         heartbeat, health, probe, cursor_get, cursor_commit (khive #449)"
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
    assert!(
        names.contains(&"comm.ingest"),
        "comm.ingest verb must be registered"
    );
    assert!(
        names.contains(&"comm.heartbeat"),
        "comm.heartbeat verb must be registered (khive #606)"
    );
    assert!(
        names.contains(&"comm.probe"),
        "comm.probe verb must be registered"
    );
    assert!(
        names.contains(&"comm.cursor_get"),
        "comm.cursor_get verb must be registered (khive #449)"
    );
    assert!(
        names.contains(&"comm.cursor_commit"),
        "comm.cursor_commit verb must be registered (khive #449)"
    );
    assert!(
        names.contains(&"comm.health"),
        "comm.health verb must be registered (khive #606)"
    );
}

#[test]
fn comm_pack_declares_channel_health_note_kind() {
    assert!(
        CommPack::NOTE_KINDS.contains(&"channel_health"),
        "khive #606: channel_health must be a pack-owned note kind"
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

    // ADR-007 Rev 2: dispatch pins token to Namespace::local(); data lives in "local".
    let caller_token = rt.authorize(Namespace::parse("local").unwrap()).unwrap();
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
        "local namespace must have exactly 1 outbound note (ADR-007 all-local); got {outbound:?}"
    );
    // ADR-007 Rev 2: `to_actor` carries the intended recipient ("lambda:khive").
    // The `to` property is caller_ns ("local") per dual_write_message's actor-addressed path.
    assert_eq!(
        outbound[0]
            .properties
            .as_ref()
            .unwrap()
            .get("to_actor")
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

    // ADR-007 Rev 2: dispatch pins token to Namespace::local(); data lives in "local".
    let caller_token = rt.authorize(Namespace::parse("local").unwrap()).unwrap();
    let notes = rt
        .list_notes(&caller_token, Some("message"), 100, 0)
        .await
        .expect("list_notes in local ns succeeds");
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
        "local namespace must have exactly 1 inbound note (ADR-007 all-local); got {inbound:?}"
    );
    let props = inbound[0].properties.as_ref().unwrap();
    // ADR-007 Rev 2: token.namespace() is always "local", so from = "local".
    assert_eq!(props.get("from").and_then(|v| v.as_str()), Some("local"));
    assert_eq!(props.get("to").and_then(|v| v.as_str()), Some("local"));
    assert_eq!(inbound[0].content, "meeting at 3pm");
    // inbound copy must carry an outbound_ref back to the outbound copy.
    assert!(
        props.get("outbound_ref").is_some(),
        "inbound note must carry outbound_ref"
    );
}

/// inbox() returns the inbound message after a self-send with configured actor identity.
///
/// A session with actor_id="lambda:khive" sends to itself; the inbound copy has
/// to_actor="lambda:khive" and is visible to the same registry's inbox (filter matches).
#[tokio::test]
async fn test_inbox_returns_inbound_for_recipient() {
    // Self-send: actor_id configured so inbox filter matches to_actor="lambda:khive".
    let (registry, _rt) = build_actor_registry(shared_backend(), "lambda:khive");
    registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:khive", "content": "you have mail", "self_send": true }),
        )
        .await
        .expect("self-send with actor identity succeeds");

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
    // from_actor is "lambda:khive" (configured actor_id, not anonymous "local").
    assert_eq!(
        props.get("from_actor").and_then(|v| v.as_str()),
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

    // ADR-007 Rev 2: dispatch pins token to Namespace::local(); data lives in "local".
    let caller_token = rt.authorize(Namespace::parse("local").unwrap()).unwrap();
    let notes = rt
        .list_notes(&caller_token, Some("message"), 100, 0)
        .await
        .expect("list_notes succeeds");
    let alive: Vec<_> = notes.iter().filter(|n| n.deleted_at.is_none()).collect();
    assert_eq!(
        alive.len(),
        2,
        "send-to-self must create exactly 2 notes in local ns (ADR-007 all-local); got {alive:?}"
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

    // ADR-007 Rev 2: reply_to = original to_actor = "lambda:khive".
    let reply_to = reply
        .get("to")
        .and_then(|v| v.as_str())
        .expect("reply returns to");
    assert_eq!(
        reply_to, "lambda:khive",
        "UE6-H1: self-send reply routes back to to_actor; got {reply_to}"
    );
    // ADR-007 Rev 2: from = token.namespace() = "local".
    let reply_from = reply
        .get("from")
        .and_then(|v| v.as_str())
        .expect("reply returns from");
    assert_eq!(
        reply_from, "local",
        "reply from must be local (ADR-007 all-local, token.namespace()=local)"
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
    // lambda:khive (configured actor_id) sends to itself, then replies.
    // This tests that reply routing works correctly with proper actor attribution.
    let backend = shared_backend();
    let (registry, _rt) = build_actor_registry(backend, "lambda:khive");

    registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:khive", "content": "meeting at 3pm", "self_send": true }),
        )
        .await
        .expect("self-send with actor identity succeeds");

    // Find the inbound copy via inbox (actor filter matches to_actor="lambda:khive").
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

    // UE6-H1: reply routes to original to_actor = "lambda:khive".
    let reply_to = reply
        .get("to")
        .and_then(|v| v.as_str())
        .expect("reply returns to");
    assert_eq!(
        reply_to, "lambda:khive",
        "UE6-H1: reply routes to original to_actor; got {reply_to}"
    );
    // from_actor is the configured actor_id, not anonymous "local".
    let reply_from = reply
        .get("from")
        .and_then(|v| v.as_str())
        .expect("reply returns from");
    assert_eq!(
        reply_from, "lambda:khive",
        "reply from must be the configured actor_id; got {reply_from}"
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
    // ADR-057 Q1: control characters are rejected by validate_actor_label.
    // A label with a tab character ('\t') must fail validation.
    let invalid_recipient = "lambda\tcontrol";

    let (registry, rt) = build_registry_for_ns("lambda:khive");

    let result = registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": invalid_recipient, "content": "should rollback" }),
        )
        .await;

    // The send must fail because the recipient label contains a control character.
    assert!(
        result.is_err(),
        "send to label with control character must fail; got {result:?}"
    );

    // Atomicity: validation rejects before any write, so no note in lambda:khive.
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
        "failed send must not leave any note in caller namespace; got {alive:?}"
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

// ── reply() delivers inbound copy alongside the outbound copy ────────────────

/// reply() must write both an outbound copy and an inbound copy within the same namespace.
///
/// Before the fix, reply() created only an outbound note via a single
/// create_note call, so inbox() would not surface the reply.
///
/// Cross-namespace send is denied (issue #481 fix).
/// Same-namespace send is used here — both copies land in the caller's namespace.
#[tokio::test]
async fn test_reply_delivers_inbound_to_recipient() {
    // lambda:khive (configured actor_id) sends to itself and replies.
    // With proper actor attribution, inbox filters correctly and reply inbounds are visible.
    let backend = shared_backend();
    let (registry, _rt) = build_actor_registry(backend, "lambda:khive");

    registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:khive", "content": "original message", "self_send": true }),
        )
        .await
        .expect("self-send with actor identity succeeds");

    // Find the inbound copy via inbox (actor filter matches to_actor="lambda:khive").
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
    // (the original inbound + the reply's inbound copy, both with to_actor="lambda:khive").
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
        "reply() must deliver an inbound copy; \
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
        "all inbox items must have direction=inbound; \
         got {inbox_after}"
    );
}

// ── thread() rejects nonexistent or non-message root ─────────────────────────

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
        "thread() with nonexistent root UUID must return an error; \
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
        "thread() with non-message root must return an error; \
         got ok with result={result:?}"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("message") || err.contains("kind"),
        "error must mention 'message' or 'kind'; got {err}"
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

// ── Regressions: inbox limit schema + invalid status ────────────────

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

    // ADR-007 Rev 2: dispatch pins token to Namespace::local(); data lives in "local".
    // Find the inbound copy — it has a different UUID from the outbound copy.
    let caller_token = rt.authorize(Namespace::parse("local").unwrap()).unwrap();
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

    // ADR-007 Rev 2: dispatch pins token to Namespace::local(); data lives in "local".
    // Find the inbound copy (direction=inbound) — it has a different UUID.
    let caller_token = rt.authorize(Namespace::parse("local").unwrap()).unwrap();
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

    // ADR-007 Rev 2: dispatch pins token to Namespace::local(); write directly to "local"
    // so that list(kind=message) dispatched through the registry sees the same data.
    let tok = rt.authorize(Namespace::parse("local").unwrap()).unwrap();

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

// ── ADR-057: actor-addressed send allows lambda↔lambda messaging ──────────────
//
// Before ADR-057, comm.send(to="lambda:leo") from lambda:khive was denied by the
// cross-namespace ACL gate (#481 fix). ADR-057 supersedes that gate for actor-
// addressed sends: both copies land in the caller's namespace (lambda:khive).
// The recipient namespace (lambda:leo) receives nothing — isolation is preserved.

#[tokio::test]
async fn test_cross_namespace_send_denied_issue_481() {
    // ADR-057: actor-addressed send must succeed even across actor boundaries.
    // Both copies land in the caller's namespace; no write to lambda:leo ns.
    let (registry, rt) = build_registry_for_ns("lambda:khive");

    let result = registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:leo", "content": "actor-addressed send" }),
        )
        .await;

    assert!(
        result.is_ok(),
        "ADR-057: actor-addressed send from lambda:khive to lambda:leo must succeed; got err: {result:?}"
    );

    // ADR-007 Rev 2: dispatch pins token to Namespace::local(); no write to lambda:leo ns.
    // Verify isolation: lambda:leo namespace has no notes.
    let recipient_token = rt
        .authorize(khive_runtime::Namespace::parse("lambda:leo").unwrap())
        .unwrap();
    let notes = rt
        .list_notes(&recipient_token, Some("message"), 100, 0)
        .await
        .expect("list_notes in recipient ns");
    assert_eq!(
        notes.len(),
        0,
        "ADR-057: no note in recipient (lambda:leo) namespace; both copies land in local ns"
    );

    // ADR-007 Rev 2: both copies land in "local" (not lambda:khive).
    let local_token = rt
        .authorize(khive_runtime::Namespace::parse("local").unwrap())
        .unwrap();
    let local_notes = rt
        .list_notes(&local_token, Some("message"), 100, 0)
        .await
        .expect("list_notes in local ns");
    let alive: Vec<_> = local_notes
        .iter()
        .filter(|n| n.deleted_at.is_none())
        .collect();
    assert_eq!(
        alive.len(),
        2,
        "ADR-057: both outbound and inbound copies must land in local ns (ADR-007 all-local); got {alive:?}"
    );

    // One outbound, one inbound.
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
        "ADR-057: local ns must have an outbound copy; got {directions:?}"
    );
    assert!(
        directions.contains(&"inbound"),
        "ADR-057: local ns must have an inbound copy; got {directions:?}"
    );

    // Actor labels must be stored on both copies.
    // ADR-007 Rev 2: from_actor = token.namespace() = "local".
    for note in &alive {
        let props = note.properties.as_ref().unwrap();
        assert_eq!(
            props.get("from_actor").and_then(|v| v.as_str()),
            Some("local"),
            "ADR-057: from_actor must be local (ADR-007 all-local, token.namespace()=local)"
        );
        assert_eq!(
            props.get("to_actor").and_then(|v| v.as_str()),
            Some("lambda:leo"),
            "ADR-057: to_actor must be lambda:leo"
        );
    }
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
        git_write: Default::default(),
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
        actor_id: None,
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
// ADR-007 Rev 2: all storage routes to "local". Both copies land in "local".
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

    // ADR-007 Rev 2: dispatch pins storage to "local" regardless of default_namespace.
    let tok = rt.authorize(Namespace::parse("local").unwrap()).unwrap();
    let notes = rt.list_notes(&tok, Some("message"), 100, 0).await.unwrap();
    let alive: Vec<_> = notes.iter().filter(|n| n.deleted_at.is_none()).collect();
    assert_eq!(
        alive.len(),
        2,
        "T1: expect 1 outbound + 1 inbound in local ns (ADR-007 all-local); got {}",
        alive.len()
    );
}

// T2 — cross-ns send is actor-addressed (ADR-057): succeeds regardless of allowlist.
// ADR-007 Rev 2: both copies land in "local" (the shared storage namespace).
// Actor labels (from_actor/to_actor) in note properties distinguish sender and recipient.
#[tokio::test]
async fn t2_send_cross_ns_denied_when_allowlist_empty() {
    let backend = shared_backend();
    let (registry_leo, rt_leo) = build_crossns_registry(Arc::clone(&backend), "lambda:leo", vec![]);
    let (_registry_khive, _rt_khive) =
        build_crossns_registry(Arc::clone(&backend), "lambda:khive", vec![]);

    // ADR-057: actor-addressed sends always succeed; allowlist no longer gates comm.send.
    let result = registry_leo
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:khive", "content": "actor-addressed" }),
        )
        .await;
    assert!(
        result.is_ok(),
        "T2: actor-addressed send must succeed even with empty allowlist; got {result:?}"
    );

    // ADR-007 Rev 2: both outbound + inbound copies land in "local" (all-local model).
    let local_tok = rt_leo
        .authorize(Namespace::parse("local").unwrap())
        .unwrap();
    let local_notes = rt_leo
        .list_notes(&local_tok, Some("message"), 100, 0)
        .await
        .unwrap();
    let alive: Vec<_> = local_notes
        .iter()
        .filter(|n| n.deleted_at.is_none())
        .collect();
    assert_eq!(
        alive.len(),
        2,
        "T2: expect 1 outbound + 1 inbound in local ns (ADR-007 all-local); got {}",
        alive.len()
    );

    // Verify actor labels on both copies.
    // ADR-007 Rev 2: from_actor is "local" (token.namespace()), to_actor is the "to" argument.
    for note in &alive {
        let from_actor = note
            .properties
            .as_ref()
            .and_then(|p| p.get("from_actor"))
            .and_then(|v| v.as_str());
        let to_actor = note
            .properties
            .as_ref()
            .and_then(|p| p.get("to_actor"))
            .and_then(|v| v.as_str());
        assert_eq!(
            from_actor,
            Some("local"),
            "T2: from_actor must be 'local' (ADR-007 all-local; token.namespace()=local)"
        );
        assert_eq!(
            to_actor,
            Some("lambda:khive"),
            "T2: to_actor must be lambda:khive on every note"
        );
    }
}

// T3 — actor-addressed send (ADR-057): both copies land in "local" (ADR-007 Rev 2).
// Actor labels from_actor/to_actor in note properties identify routing participants.
#[tokio::test]
async fn t3_send_cross_ns_delivers_when_allowed() {
    let backend = shared_backend();
    let (registry_leo, rt_leo) = build_crossns_registry(Arc::clone(&backend), "lambda:leo", vec![]);
    let (_reg_khive, _rt_khive) =
        build_crossns_registry(Arc::clone(&backend), "lambda:khive", vec![]);

    let result = registry_leo
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:khive", "content": "hello cross-ns" }),
        )
        .await;
    assert!(
        result.is_ok(),
        "T3: actor-addressed send must succeed; got {result:?}"
    );
    let val = result.unwrap();
    assert!(
        val.get("full_id").is_some(),
        "T3: response must carry full_id"
    );

    // ADR-007 Rev 2: all notes land in "local", not the sender's configured namespace.
    let local_tok = rt_leo
        .authorize(Namespace::parse("local").unwrap())
        .unwrap();
    let local_notes = rt_leo
        .list_notes(&local_tok, Some("message"), 100, 0)
        .await
        .unwrap();
    let outbound: Vec<_> = local_notes
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
    assert_eq!(outbound.len(), 1, "T3: expect 1 outbound note in local ns");
    let outbound_thread_id = outbound[0]
        .properties
        .as_ref()
        .and_then(|p| p.get("thread_id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .expect("T3: outbound note must have thread_id");

    // ADR-057: inbound copy also lands in "local" (ADR-007 all-local; not separate sender ns).
    let inbound: Vec<_> = local_notes
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
        "T3: expect 1 inbound note in local ns — ADR-007 all-local + ADR-057 actor-addressed"
    );
    let inbound_note = inbound[0];
    // Actor labels identify routing participants; from_actor = "local" (token.namespace()).
    assert_eq!(
        inbound_note
            .properties
            .as_ref()
            .and_then(|p| p.get("from_actor"))
            .and_then(|v| v.as_str()),
        Some("local"),
        "T3: inbound from_actor must be 'local' (ADR-007 all-local; token.namespace()=local)"
    );
    assert_eq!(
        inbound_note
            .properties
            .as_ref()
            .and_then(|p| p.get("to_actor"))
            .and_then(|v| v.as_str()),
        Some("lambda:khive"),
        "T3: inbound to_actor must be lambda:khive"
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

// T4 — inbound note's namespace column is "local" (ADR-007 Rev 2 all-local model).
// ADR-057 actor-addressed delivery: both copies land in "local", identified by actor labels.
#[tokio::test]
async fn t4_inbound_note_namespace_is_recipient() {
    let backend = shared_backend();
    let (registry_leo, rt_leo) = build_crossns_registry(Arc::clone(&backend), "lambda:leo", vec![]);
    let (_reg_khive, _rt_khive) =
        build_crossns_registry(Arc::clone(&backend), "lambda:khive", vec![]);

    registry_leo
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:khive", "content": "ns stamp check" }),
        )
        .await
        .expect("T4: send must succeed");

    // ADR-007 Rev 2: inbound note is in "local" (not the configured sender namespace).
    let local_tok = rt_leo
        .authorize(Namespace::parse("local").unwrap())
        .unwrap();
    let local_notes = rt_leo
        .list_notes(&local_tok, Some("message"), 100, 0)
        .await
        .unwrap();
    let inbound_note = local_notes
        .iter()
        .filter(|n| n.deleted_at.is_none())
        .find(|n| {
            n.properties
                .as_ref()
                .and_then(|p| p.get("direction"))
                .and_then(|v| v.as_str())
                == Some("inbound")
        })
        .expect("T4: must find inbound note in local ns (ADR-007 all-local)");
    assert_eq!(
        inbound_note.namespace.as_str(),
        "local",
        "T4: inbound note namespace must be 'local' (ADR-007 Rev 2 all-local model)"
    );
    // Actor label distinguishes the intended recipient.
    assert_eq!(
        inbound_note
            .properties
            .as_ref()
            .and_then(|p| p.get("to_actor"))
            .and_then(|v| v.as_str()),
        Some("lambda:khive"),
        "T4: inbound note to_actor must be lambda:khive"
    );
}

// T5 — ADR-057 §(c): actor-addressed delivery with configured identity.
//
// A sender with actor_id="lambda:khive" sends to "lambda:leo". Both copies land in
// the "local" namespace (ADR-007 all-local). The recipient (actor_id="lambda:leo")
// sees the message in their inbox because the to_actor filter matches.
//
// An anonymous caller on the same backend sees 0 messages (inbox leak closed, #199).
#[tokio::test]
async fn t5_recipient_inbox_sees_message() {
    let backend = shared_backend();
    let (registry_sender, rt_local) = build_actor_registry(Arc::clone(&backend), "lambda:khive");
    // Recipient configured with actor_id="lambda:leo" to receive messages addressed to them.
    let (registry_recipient, _rt_recipient) =
        build_actor_registry(Arc::clone(&backend), "lambda:leo");

    // Send from lambda:khive to actor label "lambda:leo".
    let send_result = registry_sender
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:leo", "content": "inbox check" }),
        )
        .await;
    assert!(
        send_result.is_ok(),
        "T5: send from 'lambda:khive' to 'lambda:leo' must succeed; got {send_result:?}"
    );

    // The inbound note has to_actor="lambda:leo" and lives in namespace "local".
    let local_tok = rt_local
        .authorize(Namespace::parse("local").unwrap())
        .unwrap();
    let all_notes = rt_local
        .list_notes(&local_tok, Some("message"), 100, 0)
        .await
        .unwrap();
    let inbound = all_notes
        .iter()
        .filter(|n| n.deleted_at.is_none())
        .filter(|n| {
            n.properties
                .as_ref()
                .and_then(|p| p.get("direction"))
                .and_then(|v| v.as_str())
                == Some("inbound")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        inbound.len(),
        1,
        "T5: expect 1 inbound note in 'local' namespace; got {}",
        inbound.len()
    );
    assert_eq!(
        inbound[0].namespace.as_str(),
        "local",
        "T5: inbound note namespace must be 'local'"
    );
    let inbound_to_actor = inbound[0]
        .properties
        .as_ref()
        .and_then(|p| p.get("to_actor"))
        .and_then(|v| v.as_str());
    assert_eq!(
        inbound_to_actor,
        Some("lambda:leo"),
        "T5: inbound note must have to_actor='lambda:leo'"
    );

    // The configured recipient (actor_id="lambda:leo") sees the message in their inbox.
    let inbox = registry_recipient
        .dispatch("comm.inbox", serde_json::json!({}))
        .await
        .expect("T5: inbox dispatch must succeed");
    let count = inbox.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
    assert!(
        count >= 1,
        "T5: 'lambda:leo' inbox must see the inbound message; got count={count}"
    );

    // Namespace isolation: both outbound + inbound copies are in "local" (ADR-007).
    let local_alive = all_notes.iter().filter(|n| n.deleted_at.is_none()).count();
    assert_eq!(
        local_alive, 2,
        "T5: 'local' namespace must hold both outbound + inbound copies; got {local_alive}"
    );
}

// T5b — ADR-057: comm.reply always writes same-namespace.
//
// Reply on a configured-actor setup proves the fail-closed reply path: after the fix,
// handle_reply ALWAYS passes caller_ns as both `from` and `to` to dual_write_message
// and always sets from_actor/to_actor. No path through handle_reply can cause
// dual_write_message to mint a token in a foreign namespace.
//
// We use actor_id="lambda:khive" (self-send to "lambda:khive") so that the inbox
// filter correctly surfaces both the original inbound and the reply inbound, both
// of which have to_actor="lambda:khive".
#[tokio::test]
async fn t5b_reply_always_writes_same_namespace() {
    let backend = shared_backend();
    let (registry_local, rt_local) = build_actor_registry(Arc::clone(&backend), "lambda:khive");

    // Self-send from lambda:khive to lambda:khive — both copies in "local".
    let send_val = registry_local
        .dispatch(
            "comm.send",
            serde_json::json!({
                "to": "lambda:khive",
                "content": "hello for reply",
                "self_send": true,
            }),
        )
        .await
        .expect("T5b: initial send must succeed");
    let outbound_id = send_val
        .get("full_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .expect("T5b: send must return full_id");

    // Find the inbound note in "local".
    let local_tok = rt_local
        .authorize(Namespace::parse("local").unwrap())
        .unwrap();
    let all_notes = rt_local
        .list_notes(&local_tok, Some("message"), 100, 0)
        .await
        .unwrap();
    let inbound_id = all_notes
        .iter()
        .filter(|n| n.deleted_at.is_none())
        .find(|n| {
            n.properties
                .as_ref()
                .and_then(|p| p.get("direction"))
                .and_then(|v| v.as_str())
                == Some("inbound")
        })
        .map(|n| n.id.as_hyphenated().to_string())
        .expect("T5b: must find inbound note in 'local'");

    // Reply from the same registry.
    let reply_result = registry_local
        .dispatch(
            "comm.reply",
            serde_json::json!({ "id": inbound_id, "content": "got it, replying" }),
        )
        .await;
    assert!(
        reply_result.is_ok(),
        "T5b: reply must succeed; got {reply_result:?}"
    );
    let reply_val = reply_result.unwrap();

    // Reply carries the same thread_id as the original send.
    let reply_thread_id = reply_val
        .get("thread_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .expect("T5b: reply must carry thread_id");
    assert_eq!(
        reply_thread_id, outbound_id,
        "T5b: reply thread_id must equal original outbound UUID"
    );

    // All four notes (outbound1, inbound1, outbound2, inbound2) are in "local".
    // No note exists in any other namespace.
    let notes_after = rt_local
        .list_notes(&local_tok, Some("message"), 100, 0)
        .await
        .unwrap();
    let alive = notes_after
        .iter()
        .filter(|n| n.deleted_at.is_none())
        .count();
    assert_eq!(
        alive, 4,
        "T5b: expect 4 notes after send + reply (2 outbound + 2 inbound); got {alive}"
    );
    for note in notes_after.iter().filter(|n| n.deleted_at.is_none()) {
        assert_eq!(
            note.namespace.as_str(),
            "local",
            "T5b: every note must be in 'local' namespace; found {}",
            note.namespace.as_str()
        );
    }

    // The inbox (actor_id="lambda:khive") sees the inbound messages (to_actor="lambda:khive").
    let inbox_after = registry_local
        .dispatch("comm.inbox", serde_json::json!({ "status": "all" }))
        .await
        .expect("T5b: inbox after reply must succeed");
    let inbox_count = inbox_after
        .get("count")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    assert!(
        inbox_count >= 1,
        "T5b: 'lambda:khive' inbox must see at least one inbound message; got {inbox_count}"
    );
}

// T6 — inbox isolation: sender does NOT see inbound copy addressed to another actor.
//
// An anonymous sender (no actor_id) sends from "lambda:leo" namespace to "lambda:khive".
// The inbound note has to_actor="lambda:khive". The sender's inbox uses EqOrMissing("local")
// filter (anonymous), so it sees 0 messages. The inbound copy is invisible to the sender.
//
// This is the CORRECT post-#199-fix behavior. The old behavior (seeing 1) was the leak.
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

    // After fix #199: anonymous sender's inbox does NOT see the inbound copy addressed
    // to "lambda:khive". EqOrMissing("local") filter returns 0 (no matching to_actor).
    let inbox = registry_leo
        .dispatch("comm.inbox", serde_json::json!({ "status": "all" }))
        .await
        .expect("T6: sender inbox dispatch must succeed");
    let count = inbox.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
    assert_eq!(
        count, 0,
        "T6: #199 fix: anonymous sender must NOT see inbound copy addressed to lambda:khive; got {count}"
    );
}

// T7 — white-box: with_namespace token scoping (realigned to ADR-007 by-ID contract, #148).
//
// `NamespaceToken::with_namespace(recipient)` produces a token scoped to the
// recipient namespace.  It is an ordinary NamespaceToken — NOT a type-enforced
// write-only capability.
//
// Under ADR-007 rule 2 (PR #148), by-ID operations are namespace-blind: the token's
// namespace is used for WRITE attribution and multi-record LIST filtering only.
// A `get_note_including_deleted` call resolves a globally-unique UUID and returns
// the record regardless of which namespace the token carries.
//
//   (a) The minted token CAN read the SENDER-namespace note by UUID (by-ID reads are
//       namespace-blind; the gate, not the token's visible set, is the auth boundary).
//   (b) The minted token CAN read the RECIPIENT-namespace note by UUID (same contract).
//
// The security boundary remains the sender-side allowlist check on comm.send;
// the token type does not enforce read isolation on by-ID fetches.
#[tokio::test]
async fn t7_with_namespace_token_scoping() {
    let backend = shared_backend();
    let (_registry_leo, rt_leo) = build_crossns_registry(
        Arc::clone(&backend),
        "lambda:leo",
        vec![Namespace::parse("lambda:khive").unwrap()],
    );
    let (_registry_khive, rt_khive) =
        build_crossns_registry(Arc::clone(&backend), "lambda:khive", vec![]);

    // Create a note in lambda:leo (sender) namespace.
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

    // Create a note in lambda:khive (recipient) namespace.
    let khive_tok = rt_khive
        .authorize(Namespace::parse("lambda:khive").unwrap())
        .unwrap();
    let recipient_note = rt_khive
        .create_note(
            &khive_tok,
            "observation",
            None,
            "recipient-ns note",
            None,
            None,
            vec![],
        )
        .await
        .expect("T7: create recipient note");

    // Mint the kind of token that with_namespace produces (recipient-scoped).
    let recipient_tok: NamespaceToken =
        leo_tok.with_namespace(Namespace::parse("lambda:khive").unwrap());

    // (a) By-ID reads are namespace-blind (ADR-007 rule 2, PR #148): the minted
    // token CAN read a sender-namespace note by UUID. The stored namespace of
    // the returned note must still reflect where it was created (lambda:leo).
    let can_see_sender = rt_leo
        .get_note_including_deleted(&recipient_tok, sender_note.id)
        .await;
    match can_see_sender {
        Ok(Some(note)) => {
            // Expected: by-ID fetch ignores the token's namespace; record is returned.
            // The note's own namespace must be the write namespace it was created in.
            assert_eq!(
                note.namespace, "lambda:leo",
                "T7(a): stored namespace must be the sender's write namespace"
            );
        }
        Ok(None) => panic!(
            "T7(a): by-ID read must return the sender-ns note regardless of token namespace \
             (ADR-007 rule 2, PR #148 removed by-ID namespace enforcement)"
        ),
        Err(e) => panic!("T7(a): unexpected error {e:?}"),
    }

    // (b) Minted token CAN read the recipient-ns note — it is a full read+write token
    // for the recipient ns; by-ID reads are namespace-blind in any case (#148).
    let can_see_recipient = rt_khive
        .get_note_including_deleted(&recipient_tok, recipient_note.id)
        .await;
    match can_see_recipient {
        Ok(Some(note)) => {
            // Expected: the minted token can read from its own namespace (lambda:khive).
            assert_eq!(
                note.namespace, "lambda:khive",
                "T7(b): stored namespace must be the recipient's write namespace"
            );
        }
        Ok(None) => panic!("T7(b): minted token must be able to read recipient-ns note"),
        Err(e) => panic!("T7(b): unexpected error {e:?}"),
    }
}

// T8 — ADR-007 Rev 2: inbound note is in "local" (all-local model).
// A local-namespace token CAN read the inbound note. No separate recipient namespace exists.
// Recipient isolation is provided by actor labels (to_actor), not namespace partitioning.
#[tokio::test]
async fn t8_sender_token_cannot_mutate_recipient_inbound_note() {
    let backend = shared_backend();
    let (registry_leo, rt_leo) = build_crossns_registry(Arc::clone(&backend), "lambda:leo", vec![]);
    let (_reg_khive, _rt_khive) =
        build_crossns_registry(Arc::clone(&backend), "lambda:khive", vec![]);

    registry_leo
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:khive", "content": "append-only check" }),
        )
        .await
        .expect("T8: send must succeed");

    // ADR-007 Rev 2: inbound note is in "local" (not the configured sender namespace).
    let local_tok = rt_leo
        .authorize(Namespace::parse("local").unwrap())
        .unwrap();
    let local_notes = rt_leo
        .list_notes(&local_tok, Some("message"), 100, 0)
        .await
        .unwrap();
    let inbound_id = local_notes
        .iter()
        .filter(|n| n.deleted_at.is_none())
        .find(|n| {
            n.properties
                .as_ref()
                .and_then(|p| p.get("direction"))
                .and_then(|v| v.as_str())
                == Some("inbound")
        })
        .map(|n| n.id)
        .expect("T8: must find inbound note in local ns (ADR-007 all-local)");

    // A local-namespace token CAN read the inbound note (it lives in local).
    let can_read = rt_leo
        .get_note_including_deleted(&local_tok, inbound_id)
        .await;
    match can_read {
        Ok(Some(_)) => {}
        Ok(None) => panic!("T8: local token must be able to read local-ns inbound note"),
        Err(e) => panic!("T8: unexpected error reading inbound note: {e:?}"),
    }

    // Verify actor label marks the intended recipient on the inbound copy.
    let inbound_note = local_notes
        .iter()
        .filter(|n| n.deleted_at.is_none())
        .find(|n| {
            n.properties
                .as_ref()
                .and_then(|p| p.get("direction"))
                .and_then(|v| v.as_str())
                == Some("inbound")
        })
        .unwrap();
    assert_eq!(
        inbound_note
            .properties
            .as_ref()
            .and_then(|p| p.get("to_actor"))
            .and_then(|v| v.as_str()),
        Some("lambda:khive"),
        "T8: inbound note to_actor must be lambda:khive (actor label isolation)"
    );
}

// T9 — actor-addressed reply (ADR-057) with ADR-007 Rev 2 all-local model.
//
// ADR-007: all writes go to "local". ADR-057: actor labels distinguish routing.
// example actor (registry_shared) sends to khive (both copies in "local"). Then replies to
// the inbound copy, verifying reply inherits the canonical thread_id.
#[tokio::test]
async fn t9_reply_cross_ns_delivers_when_allowed() {
    let backend = shared_backend();
    // Both actors use a registry with default_namespace="lambda:shared", but ADR-007
    // ensures all storage routes to "local".
    let (registry_shared, rt_shared) =
        build_crossns_registry(Arc::clone(&backend), "lambda:shared", vec![]);

    // "example actor" (operating as lambda:shared) sends to "khive".
    let send_result = registry_shared
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

    // ADR-007 Rev 2: all notes are in "local", not "lambda:shared".
    let local_tok = rt_shared
        .authorize(Namespace::parse("local").unwrap())
        .unwrap();
    let all_notes = rt_shared
        .list_notes(&local_tok, Some("message"), 100, 0)
        .await
        .unwrap();
    let inbound_id = all_notes
        .iter()
        .filter(|n| n.deleted_at.is_none())
        .find(|n| {
            n.properties
                .as_ref()
                .and_then(|p| p.get("direction"))
                .and_then(|v| v.as_str())
                == Some("inbound")
        })
        .map(|n| n.id.as_hyphenated().to_string())
        .expect("T9: must find inbound note in local ns (ADR-007 all-local)");

    // Reply to the inbound message using the same registry.
    let reply_result = registry_shared
        .dispatch(
            "comm.reply",
            serde_json::json!({ "id": inbound_id, "content": "got it" }),
        )
        .await;
    assert!(
        reply_result.is_ok(),
        "T9: reply must succeed; got {reply_result:?}"
    );
    let reply_val = reply_result.unwrap();

    // Reply carries the same thread_id as the original send.
    let reply_thread_id = reply_val
        .get("thread_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .expect("T9: reply response must carry thread_id");
    assert_eq!(
        reply_thread_id, outbound_thread_id,
        "T9: reply thread_id must match original outbound UUID"
    );

    // Four notes in local ns: outbound1 + inbound1 + outbound2 (reply) + inbound2.
    let notes_after = rt_shared
        .list_notes(&local_tok, Some("message"), 100, 0)
        .await
        .unwrap();
    let alive = notes_after
        .iter()
        .filter(|n| n.deleted_at.is_none())
        .count();
    assert_eq!(
        alive, 4,
        "T9: expect 4 notes in local ns after send + reply (2 outbound + 2 inbound); got {alive}"
    );
}

// T10 — ADR-057: reply to a non-existent message ID fails with NotFound.
// Under actor-addressed delivery, the inbound note is in the SENDER's namespace
// (lambda:leo), not the recipient's (lambda:khive). A reply attempt by khive
// using a random ID fails because the note is not visible in khive's namespace.
#[tokio::test]
async fn t10_reply_cross_ns_denied_when_empty() {
    let backend = shared_backend();
    let (registry_leo, _rt_leo) =
        build_crossns_registry(Arc::clone(&backend), "lambda:leo", vec![]);
    let (registry_khive, _rt_khive) =
        build_crossns_registry(Arc::clone(&backend), "lambda:khive", vec![]);

    // leo sends to khive — both copies land in leo ns, khive ns gets nothing.
    registry_leo
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:khive", "content": "setup for T10" }),
        )
        .await
        .expect("T10: initial send must succeed");

    // khive attempts to reply using a well-formed but non-existent UUID.
    // The inbound note is in lambda:leo ns, invisible to lambda:khive registry.
    let nonexistent_id = "00000000-0000-0000-0000-000000000000";
    let reply_result = registry_khive
        .dispatch(
            "comm.reply",
            serde_json::json!({ "id": nonexistent_id, "content": "attempt reply to unknown" }),
        )
        .await;
    assert!(
        reply_result.is_err(),
        "T10: reply to non-existent message must fail"
    );
    let err_str = reply_result.unwrap_err().to_string();
    assert!(
        err_str.contains("not found")
            || err_str.contains("NotFound")
            || err_str.contains("no record"),
        "T10: error must indicate not found; got {err_str:?}"
    );
}

// T11 — ADR-057: actor-addressed send always succeeds (allowlist no longer gates comm.send).
// ADR-007 Rev 2: both notes land in "local" (all-local model).
// The rollback path (dual_write_message) is tested by T13/T14 via FTS/vector injection.
#[tokio::test]
async fn t11_inbound_write_failure_rolls_back_outbound() {
    let backend = shared_backend();
    let (registry_leo, rt_leo) = build_crossns_registry(Arc::clone(&backend), "lambda:leo", vec![]);

    // ADR-057: send is actor-addressed and always succeeds regardless of allowlist.
    let result = registry_leo
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:khive", "content": "actor-addressed always succeeds" }),
        )
        .await;
    assert!(
        result.is_ok(),
        "T11: actor-addressed send must succeed; got {result:?}"
    );

    // ADR-007 Rev 2: both outbound + inbound copies land in "local" (not sender ns).
    let local_tok = rt_leo
        .authorize(Namespace::parse("local").unwrap())
        .unwrap();
    let local_notes = rt_leo
        .list_notes(&local_tok, Some("message"), 100, 0)
        .await
        .unwrap();
    let alive = local_notes
        .iter()
        .filter(|n| n.deleted_at.is_none())
        .count();
    assert_eq!(
        alive, 2,
        "T11: expect 1 outbound + 1 inbound in local ns (ADR-007 all-local); got {alive}"
    );
}

// T12 — ADR-057: both directions succeed (actor-addressed, allowlist no longer gates comm.send).
// ADR-007 Rev 2: each send produces 2 notes in "local" (all-local model).
// After 2 sends (leo→khive and khive→leo), "local" has 4 notes total.
// Actor labels distinguish the sender/recipient for each pair.
#[tokio::test]
async fn t12_allowlist_is_one_directional() {
    let backend = shared_backend();
    let (registry_leo, rt_leo) = build_crossns_registry(Arc::clone(&backend), "lambda:leo", vec![]);
    let (registry_khive, rt_khive) =
        build_crossns_registry(Arc::clone(&backend), "lambda:khive", vec![]);

    // leo → khive: succeeds.
    let result_leo = registry_leo
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:khive", "content": "leo to khive" }),
        )
        .await;
    assert!(
        result_leo.is_ok(),
        "T12: leo→khive send must succeed under ADR-057; got {result_leo:?}"
    );

    // ADR-007 Rev 2: notes from leo's send are in "local" (rt_leo's backend).
    let local_tok_leo = rt_leo
        .authorize(Namespace::parse("local").unwrap())
        .unwrap();
    let leo_local_notes = rt_leo
        .list_notes(&local_tok_leo, Some("message"), 100, 0)
        .await
        .unwrap();
    assert_eq!(
        leo_local_notes
            .iter()
            .filter(|n| n.deleted_at.is_none())
            .count(),
        2,
        "T12: 2 notes (outbound+inbound) in local ns after leo→khive send"
    );

    // khive → leo: also succeeds.
    let result_khive = registry_khive
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:leo", "content": "reverse direction" }),
        )
        .await;
    assert!(
        result_khive.is_ok(),
        "T12: khive→leo send must succeed under ADR-057; got {result_khive:?}"
    );

    // ADR-007 Rev 2: notes from khive's send are also in "local" (shared backend).
    let local_tok_khive = rt_khive
        .authorize(Namespace::parse("local").unwrap())
        .unwrap();
    let khive_local_notes = rt_khive
        .list_notes(&local_tok_khive, Some("message"), 100, 0)
        .await
        .unwrap();
    // Both registries share the same backend, so local has 4 notes total (2 per send).
    assert_eq!(
        khive_local_notes
            .iter()
            .filter(|n| n.deleted_at.is_none())
            .count(),
        4,
        "T12: 4 notes total in local ns after both sends (ADR-007 all-local, shared backend)"
    );
}

// T13 — FTS failure on note write leaves no stranded row.
//
// Under ADR-007 Rev 2 dispatch pins the storage token to Namespace::local().
// arm_fts_fail("local") would race against every other concurrent test that
// writes a note to "local". To preserve namespace-targeting isolation, this
// test uses a unique UUID namespace via rt.create_note() directly (bypassing
// dispatch). This validates the same create_note_inner rollback behavior —
// commit row → FTS error → compensate (delete row) → return Err — without
// the cross-test injection race that "local" would introduce.
#[tokio::test]
async fn t13_inbound_fts_failure_leaves_no_stranded_row() {
    use khive_runtime::arm_fts_fail;

    // Unique namespace keeps the process-global FTS_FAIL_NS one-shot isolated
    // from other concurrent tests (each test uses a different UUID).
    let unique_ns = format!("t13-{}", uuid::Uuid::new_v4().simple());
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let tok = rt.authorize(Namespace::parse(&unique_ns).unwrap()).unwrap();

    // Arm FTS injection on the unique namespace — fires on the next create_note
    // call in this namespace, then clears (one-shot).
    arm_fts_fail(&unique_ns);

    // Attempt to create a note; the FTS step must fail and roll back the row.
    let result = rt
        .create_note(
            &tok,
            "message",
            None,
            "t13 fts-fail test",
            None,
            Some(serde_json::json!({ "direction": "outbound" })),
            vec![],
        )
        .await;
    assert!(
        result.is_err(),
        "T13: create_note must fail when FTS injection is armed; got: {result:?}"
    );

    // No live note must remain — the row was compensated by create_note_inner.
    let notes = rt.list_notes(&tok, Some("message"), 100, 0).await.unwrap();
    let alive = notes.iter().filter(|n| n.deleted_at.is_none()).count();
    assert_eq!(
        alive, 0,
        "T13: no stranded note after FTS failure (create_note_inner must compensate); got {alive}"
    );
}

// T14 — vector insertion failure on note write leaves no stranded row.
//
// Under ADR-007 Rev 2 dispatch pins the storage token to Namespace::local().
// arm_vector_fail("local") would race against every other concurrent test that
// writes a note with an embedder registered. To preserve namespace-targeting
// isolation, this test uses a unique UUID namespace via rt.create_note()
// directly (bypassing dispatch). This validates the same create_note_inner
// rollback behavior — commit row → FTS ok → vector error → compensate (delete
// row + FTS) → return Err — without the cross-test injection race.
#[tokio::test]
async fn t14_inbound_vector_failure_leaves_no_stranded_row() {
    use async_trait::async_trait;
    use khive_runtime::{arm_vector_fail, EmbedderProvider};
    use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};

    const T14_MODEL: &str = "t14-const-vec";
    const T14_DIMS: usize = 4;

    struct T14VecService;
    #[async_trait]
    impl EmbeddingService for T14VecService {
        async fn embed(
            &self,
            texts: &[String],
            _model: EmbeddingModel,
        ) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
            Ok(texts.iter().map(|_| vec![1.0_f32; T14_DIMS]).collect())
        }
        fn supports_model(&self, _model: EmbeddingModel) -> bool {
            true
        }
        fn name(&self) -> &'static str {
            "t14-const-vec"
        }
    }

    struct T14VecProvider;
    #[async_trait]
    impl EmbedderProvider for T14VecProvider {
        fn name(&self) -> &str {
            T14_MODEL
        }
        fn dimensions(&self) -> usize {
            T14_DIMS
        }
        async fn build(&self) -> khive_runtime::RuntimeResult<Arc<dyn EmbeddingService>> {
            Ok(Arc::new(T14VecService))
        }
    }

    // Unique namespace keeps the process-global VECTOR_FAIL_NS one-shot isolated
    // from other concurrent tests (each test uses a different UUID).
    let unique_ns = format!("t14-{}", uuid::Uuid::new_v4().simple());
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    rt.register_embedder(T14VecProvider);
    let tok = rt.authorize(Namespace::parse(&unique_ns).unwrap()).unwrap();

    // Arm vector injection on the unique namespace — fires on the next create_note
    // call in this namespace after row + FTS commit, then clears (one-shot).
    arm_vector_fail(&unique_ns);

    // Attempt to create a note; the vector step must fail and roll back row + FTS.
    let result = rt
        .create_note(
            &tok,
            "message",
            None,
            "t14 vec-fail test",
            None,
            Some(serde_json::json!({ "direction": "outbound" })),
            vec![],
        )
        .await;
    assert!(
        result.is_err(),
        "T14: create_note must fail when vector injection is armed; got: {result:?}"
    );

    // No live note must remain — the row was compensated by create_note_inner.
    let notes = rt.list_notes(&tok, Some("message"), 100, 0).await.unwrap();
    let alive = notes.iter().filter(|n| n.deleted_at.is_none()).count();
    assert_eq!(
        alive, 0,
        "T14: no stranded note after vector failure (create_note_inner must compensate); got {alive}"
    );
}

// ── Issue #75 regression: actor-identity filter (ADR-057) ────────────────────
//
// Root cause: handle_inbox read caller_actor from token.namespace() (always
// "local") instead of token.actor().id. The to_actor guard was permanently
// dormant. After the fix, when RuntimeConfig.actor_id is set, authorize() mints
// a token carrying that actor label, activating the filter.

/// Build a comm registry backed by a shared in-memory StorageBackend with a
/// configured actor identity. The minted token's actor.id will equal `actor_id`,
/// activating the to_actor filter in handle_inbox.
fn build_actor_registry(
    backend: Arc<khive_db::StorageBackend>,
    actor_id: &str,
) -> (VerbRegistry, KhiveRuntime) {
    let config = RuntimeConfig {
        git_write: Default::default(),
        db_path: None,
        default_namespace: Namespace::local(),
        embedding_model: None,
        additional_embedding_models: vec![],
        gate: Arc::new(AllowAllGate),
        packs: vec!["kg".to_string(), "comm".to_string()],
        backend_id: BackendId::main(),
        brain_profile: None,
        visible_namespaces: vec![],
        allowed_outbound_namespaces: vec![],
        actor_id: Some(actor_id.to_string()),
    };
    let rt = KhiveRuntime::from_backend(backend, config);
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(rt.clone()));
    builder.register(CommPack::new(rt.clone()));
    builder.with_actor_id(Some(actor_id.to_string()));
    let registry = builder.build().expect("actor registry builds");
    (registry, rt)
}

/// Actor A sends to actor B. B's inbox should see the message; A's inbox should not.
#[tokio::test]
async fn t_actor_inbox_filters_to_actor() {
    let backend = shared_backend();

    let (registry_a, _rt_a) = build_actor_registry(backend.clone(), "lambda:a");
    let (registry_b, _rt_b) = build_actor_registry(backend.clone(), "lambda:b");

    // A sends to B.
    let send_result = registry_a
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:b", "content": "hello B from A" }),
        )
        .await
        .expect("send succeeds");
    assert!(
        send_result.get("id").is_some(),
        "send must return id: {send_result}"
    );

    // B's inbox (status=all) should contain exactly one message addressed to lambda:b.
    // Note: comm.inbox already filters for direction=inbound; all returned messages are inbound.
    let b_inbox = registry_b
        .dispatch(
            "comm.inbox",
            serde_json::json!({ "status": "all", "limit": 50 }),
        )
        .await
        .expect("B inbox succeeds");
    let b_count = b_inbox["count"].as_u64().unwrap_or(0);
    let b_messages = b_inbox["messages"].as_array().expect("messages array");
    assert_eq!(
        b_count,
        1,
        "B must see exactly 1 message (addressed to lambda:b); count={b_count}, messages: {b_messages:?}"
    );
    // Verify the message is addressed to lambda:b (via the properties.to_actor field).
    let b_to_actor = b_messages[0]["properties"]["to_actor"].as_str();
    assert_eq!(
        b_to_actor,
        Some("lambda:b"),
        "message must be addressed to lambda:b; got {b_to_actor:?}"
    );

    // A's inbox should NOT contain the message (it was addressed to lambda:b, not lambda:a).
    let a_inbox = registry_a
        .dispatch(
            "comm.inbox",
            serde_json::json!({ "status": "all", "limit": 50 }),
        )
        .await
        .expect("A inbox succeeds");
    let a_count = a_inbox["count"].as_u64().unwrap_or(0);
    assert_eq!(
        a_count, 0,
        "A must see 0 messages (message was addressed to B, not A); got {a_count}"
    );
}

/// After fix #199, an anonymous caller's inbox is filtered to messages with to_actor="local"
/// or absent. Messages sent to specific actor labels (e.g. "lambda:x", "lambda:y") are
/// NOT visible to anonymous callers — this closes the cross-actor inbox read leak.
///
/// Prior behavior (pre-fix): all messages were visible to anonymous callers ("party-line
/// fallback"). That behavior was the bug.
#[tokio::test]
async fn t_anonymous_actor_inbox_filters_addressed_messages() {
    let (registry, _rt) = build_registry();

    // Send two messages from the same anonymous session to specific actor labels.
    // These sends emit a tracing::warn! (#200) but proceed.
    registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:x", "content": "msg 1" }),
        )
        .await
        .expect("send 1");
    registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:y", "content": "msg 2" }),
        )
        .await
        .expect("send 2");

    // Anonymous inbox must NOT see messages addressed to specific actors.
    // Only messages with to_actor="local" or absent are visible (EqOrMissing filter).
    let inbox = registry
        .dispatch(
            "comm.inbox",
            serde_json::json!({ "status": "all", "limit": 50 }),
        )
        .await
        .expect("inbox succeeds");
    let count = inbox["count"].as_u64().unwrap_or(0);
    assert_eq!(
        count, 0,
        "#199: anonymous inbox must NOT show messages addressed to lambda:x/lambda:y; got {count}"
    );
}

/// TOML wiring: actor.id in khive.toml flows into RuntimeConfig.actor_id.
#[test]
fn t_actor_id_wires_from_toml_into_runtime_config() {
    use khive_runtime::{runtime_config_from_khive_config, RuntimeConfig};

    let toml_src = r#"
[actor]
id = "lambda:khive"
"#;
    let khive_cfg: khive_runtime::KhiveConfig = toml::from_str(toml_src).expect("TOML must parse");
    let base = RuntimeConfig::default();
    let resolved = runtime_config_from_khive_config(&khive_cfg, base);
    assert_eq!(
        resolved.actor_id.as_deref(),
        Some("lambda:khive"),
        "actor.id must flow through to RuntimeConfig.actor_id"
    );
}

/// TOML wiring: absent actor.id leaves RuntimeConfig.actor_id as None.
#[test]
fn t_absent_actor_id_leaves_runtime_config_actor_id_none() {
    use khive_runtime::{runtime_config_from_khive_config, RuntimeConfig};

    let toml_src = r#"
[actor]
allowed_outbound_namespaces = ["lambda:other"]
"#;
    let khive_cfg: khive_runtime::KhiveConfig = toml::from_str(toml_src).expect("TOML must parse");
    let base = RuntimeConfig::default();
    let resolved = runtime_config_from_khive_config(&khive_cfg, base);
    assert!(
        resolved.actor_id.is_none(),
        "absent actor.id must leave actor_id as None; got {:?}",
        resolved.actor_id
    );
}

// TOML wiring test — KhiveConfig parsed from TOML with
// `actor.allowed_outbound_namespaces = [...]` must land those values in
// RuntimeConfig.allowed_outbound_namespaces.
#[test]
fn toml_allowed_outbound_namespaces_wires_into_runtime_config() {
    use khive_runtime::{runtime_config_from_khive_config, RuntimeConfig};

    let toml_src = r#"
[actor]
id = "lambda:leo"
allowed_outbound_namespaces = ["lambda:khive", "lambda:atlas"]
"#;
    let khive_cfg: khive_runtime::KhiveConfig = toml::from_str(toml_src).expect("TOML must parse");

    let base = RuntimeConfig {
        git_write: Default::default(),
        db_path: None,
        embedding_model: None,
        additional_embedding_models: vec![],
        packs: vec!["kg".to_string(), "comm".to_string()],
        ..RuntimeConfig::default()
    };
    let resolved = runtime_config_from_khive_config(&khive_cfg, base);

    let outbound_strs: Vec<&str> = resolved
        .allowed_outbound_namespaces
        .iter()
        .map(|ns| ns.as_str())
        .collect();

    assert!(
        outbound_strs.contains(&"lambda:khive"),
        "allowed_outbound_namespaces must contain 'lambda:khive'; got {outbound_strs:?}"
    );
    assert!(
        outbound_strs.contains(&"lambda:atlas"),
        "allowed_outbound_namespaces must contain 'lambda:atlas'; got {outbound_strs:?}"
    );
    assert_eq!(
        outbound_strs.len(),
        2,
        "exactly 2 outbound namespaces expected; got {outbound_strs:?}"
    );
}

// ---------------------------------------------------------------------------
// Cluster-2 isolation tests (branch fix/comm-tenant-isolation-strict)
// ---------------------------------------------------------------------------
//
// These tests were added as part of the pre-cloud-hardening audit (2026-06-23).
// They cover the decision-independent (no ADR required) half of the isolation
// story:
//   - #199 (comm.inbox actor-filter bypass): the to_actor filter must isolate
//     tenants when actor_id IS configured.
//   - #224 (gate actor identity gap): the GateRequest.actor must carry the
//     configured actor identity, not ActorRef::anonymous(), so a cloud TenantGate
//     can act on it. Fixed in PR #271, which removed the #[ignore] attribute.
//     The test now passes unconditionally.

/// When two registries share the same storage backend but carry distinct
/// configured actor identities, `comm.inbox` must isolate each actor's view:
///
/// - Actor A sends to B → B's inbox (status=all) shows the message; A's does not.
/// - Actor B sends to A → A's inbox (status=all) shows the message; B's does not.
///
/// This is the end-to-end isolation assertion for #199. It PASSES today when
/// `actor_id` is properly configured per-registry. The test documents that the
/// to_actor filter is active and working — the misconfiguration footgun (missing
/// actor_id → party-line) is addressed separately by the strict-mode startup gate
/// (`KHIVE_REQUIRE_ATTRIBUTED_ACTOR=1`).
#[tokio::test]
async fn t_c2_inbox_isolation_cross_actor() {
    let backend = shared_backend();

    let (registry_a, _rt_a) = build_actor_registry(backend.clone(), "lambda:tenant-a");
    let (registry_b, _rt_b) = build_actor_registry(backend.clone(), "lambda:tenant-b");

    // A sends to B.
    let send_ab = registry_a
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:tenant-b", "content": "hello tenant-b from a" }),
        )
        .await
        .expect("A→B send must succeed");
    assert!(
        send_ab.get("id").is_some(),
        "send must return id: {send_ab}"
    );

    // B sends to A.
    let send_ba = registry_b
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:tenant-a", "content": "hello tenant-a from b" }),
        )
        .await
        .expect("B→A send must succeed");
    assert!(
        send_ba.get("id").is_some(),
        "send must return id: {send_ba}"
    );

    // B's inbox must contain exactly one message (the one addressed to tenant-b).
    let b_inbox = registry_b
        .dispatch(
            "comm.inbox",
            serde_json::json!({ "status": "all", "limit": 50 }),
        )
        .await
        .expect("B inbox must succeed");
    let b_count = b_inbox["count"].as_u64().unwrap_or(0);
    assert_eq!(
        b_count, 1,
        "B must see exactly 1 message (the A→B message); got {b_count}. \
         If this is > 1, the to_actor filter is not applied (party-line leak)."
    );
    let b_content = b_inbox["messages"][0]["content"].as_str().unwrap_or("");
    assert_eq!(
        b_content, "hello tenant-b from a",
        "B's inbox message must be the one A sent to B; got {b_content:?}"
    );

    // A's inbox must contain exactly one message (the one addressed to tenant-a).
    let a_inbox = registry_a
        .dispatch(
            "comm.inbox",
            serde_json::json!({ "status": "all", "limit": 50 }),
        )
        .await
        .expect("A inbox must succeed");
    let a_count = a_inbox["count"].as_u64().unwrap_or(0);
    assert_eq!(
        a_count, 1,
        "A must see exactly 1 message (the B→A message); got {a_count}. \
         If this is > 1, the to_actor filter is not applied (party-line leak)."
    );
    let a_content = a_inbox["messages"][0]["content"].as_str().unwrap_or("");
    assert_eq!(
        a_content, "hello tenant-a from b",
        "A's inbox message must be the one B sent to A; got {a_content:?}"
    );
}

/// Verifies that the configured actor identity reaches the gate (issue #224 fix).
///
/// When `actor_id = "lambda:tenant-x"` is set on the `VerbRegistryBuilder`, the
/// `GateRequest.actor.id` must equal `"lambda:tenant-x"` so that a cloud
/// `TenantGate` can enforce per-actor policies. Fixed in PR #234 by threading the
/// configured actor into `VerbRegistry::dispatch` before the gate consult.
///
/// See: https://github.com/ohdearquant/khive/issues/224
#[tokio::test]
async fn t_c2_gate_receives_configured_actor_not_anonymous() {
    use khive_runtime::{Gate, GateDecision, GateError, GateRef, GateRequest};
    use std::sync::Mutex;

    // A recording gate that captures every actor ID it sees.
    #[derive(Debug)]
    struct RecordingGate {
        seen_actor_ids: Mutex<Vec<String>>,
    }

    impl Gate for RecordingGate {
        fn check(&self, req: &GateRequest) -> Result<GateDecision, GateError> {
            self.seen_actor_ids
                .lock()
                .unwrap()
                .push(req.actor.id.clone());
            Ok(GateDecision::allow())
        }
    }

    let gate = Arc::new(RecordingGate {
        seen_actor_ids: Mutex::new(Vec::new()),
    });

    let backend = shared_backend();
    let config = RuntimeConfig {
        git_write: Default::default(),
        db_path: None,
        default_namespace: Namespace::local(),
        embedding_model: None,
        additional_embedding_models: vec![],
        gate: Arc::new(AllowAllGate), // runtime gate; registry gate set below
        packs: vec!["kg".to_string(), "comm".to_string()],
        backend_id: BackendId::main(),
        brain_profile: None,
        visible_namespaces: vec![],
        allowed_outbound_namespaces: vec![],
        actor_id: Some("lambda:tenant-x".to_string()),
    };
    let rt = KhiveRuntime::from_backend(backend, config);
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(rt.clone()));
    builder.register(CommPack::new(rt.clone()));
    builder.with_actor_id(Some("lambda:tenant-x".to_string()));
    builder.with_gate(gate.clone() as GateRef);
    let registry = builder.build().expect("registry with recording gate");

    // Dispatch any verb — the gate is consulted before pack dispatch.
    let _ = registry
        .dispatch(
            "comm.inbox",
            serde_json::json!({ "status": "all", "limit": 1 }),
        )
        .await
        .expect("inbox dispatch must not error");

    // The gate must have recorded the configured "lambda:tenant-x" actor, not
    // "local" (anonymous). #234 threads the configured actor into dispatch so
    // the gate sees it before the consult; a regression would record "local".
    let seen = gate.seen_actor_ids.lock().unwrap();
    assert!(
        seen.iter().any(|id| id == "lambda:tenant-x"),
        "gate must receive configured actor id 'lambda:tenant-x', not 'local' \
         (anonymous). Saw: {seen:?}. \
         Fix: pass actor_id into GateRequest at pack.rs:852 instead of \
         ActorRef::anonymous(). Tracked as issue #224."
    );
}

// ── Issue #199 / #200 regression: actor attribution and inbox isolation ────────
//
// These tests reproduce the two bugs fixed in this PR:
//
// #200: from_actor stamped as 'local' when sender has no actor.id configured but
//       sends to a specific actor label.  Addressed sends from anonymous callers
//       must be rejected; party-line self-sends (to="local") still work.
//
// #199: inbox actor-filter skipped when caller resolves to anonymous/'local'.
//       An unconfigured caller must NOT see messages addressed to other actors;
//       they must only see messages whose to_actor is "local" (or absent/NULL).

/// #200 regression: anonymous sender sending to a specific actor label stamps from_actor="local".
///
/// The send is NOT rejected (to preserve backward compatibility with sessions that set
/// default_namespace but not actor_id), but attribution is mis-stamped. A tracing::warn!
/// is emitted. This is a known limitation pending issue #75 (actor identity per request).
///
/// The important invariant: even with the corrupted from_actor, the message is stored and
/// the #199 inbox fix prevents OTHER anonymous callers from reading messages with
/// to_actor set to a specific label.
#[tokio::test]
async fn i199_200_anonymous_send_to_specific_actor_is_warned() {
    // build_registry() has no actor_id → token.actor().id = "local".
    let (registry, _rt) = build_registry();

    let result = registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:leo", "content": "mis-attributed send" }),
        )
        .await;

    // The send proceeds (warn-only), not rejected.
    assert!(
        result.is_ok(),
        "#200: anonymous send to a specific actor must proceed (warn-only); got err: {result:?}"
    );
    let resp = result.unwrap();
    assert!(
        resp.get("id").is_some(),
        "#200: response must carry id for the stored message"
    );
}

/// #200 / single-tenant: anonymous sender sending to "local" (party-line) still works.
///
/// The fix must not break OSS single-tenant deployments where everyone is 'local'.
#[tokio::test]
async fn i199_200_anonymous_send_to_local_still_works() {
    // build_registry() has no actor_id → anonymous caller.
    let (registry, _rt) = build_registry();

    let result = registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "local", "content": "party-line message" }),
        )
        .await;

    assert!(
        result.is_ok(),
        "#200 single-tenant: anonymous send to 'local' must still work; got err: {result:?}"
    );
    assert!(
        result.unwrap().get("id").is_some(),
        "#200 single-tenant: response must carry id"
    );
}

/// #199 regression: anonymous caller must NOT read messages addressed to other actors.
///
/// Before the fix, `comm.inbox` with an unconfigured caller (actor="local") returned
/// ALL inbound messages regardless of `to_actor`, leaking cross-actor inbox content.
/// After the fix, the anonymous caller only sees messages with to_actor="local" or
/// to_actor absent/NULL.
#[tokio::test]
async fn i199_anonymous_inbox_cannot_read_messages_addressed_to_other_actor() {
    let backend = shared_backend();

    // Actor B (configured) sends a message addressed to itself.  We inject the
    // inbound note directly to give it to_actor="lambda:b" without going through
    // the send gate that would now reject an anonymous send.
    let (registry_b, _rt_b) = build_actor_registry(backend.clone(), "lambda:b");
    registry_b
        .dispatch(
            "comm.send",
            serde_json::json!({
                "to": "lambda:b",
                "content": "secret for B only",
                "self_send": true,
            }),
        )
        .await
        .expect("B sends to itself");

    // Confirm B can read its own inbox (1 message).
    let b_inbox = registry_b
        .dispatch("comm.inbox", serde_json::json!({ "status": "all" }))
        .await
        .expect("B inbox");
    let b_count = b_inbox["count"].as_u64().unwrap_or(0);
    assert_eq!(
        b_count, 1,
        "#199: B must see 1 message addressed to lambda:b"
    );

    // An anonymous (unconfigured) caller on the same backend must NOT see B's message.
    let config_anon = RuntimeConfig {
        git_write: Default::default(),
        db_path: None,
        default_namespace: Namespace::local(),
        embedding_model: None,
        additional_embedding_models: vec![],
        gate: Arc::new(AllowAllGate),
        packs: vec!["kg".to_string(), "comm".to_string()],
        backend_id: BackendId::main(),
        brain_profile: None,
        visible_namespaces: vec![],
        allowed_outbound_namespaces: vec![],
        actor_id: None, // anonymous
    };
    let rt_anon = KhiveRuntime::from_backend(backend, config_anon);
    let mut builder_anon = VerbRegistryBuilder::new();
    builder_anon.register(khive_pack_kg::KgPack::new(rt_anon.clone()));
    builder_anon.register(CommPack::new(rt_anon.clone()));
    // No with_actor_id → anonymous.
    let registry_anon = builder_anon.build().expect("anon registry");

    let anon_inbox = registry_anon
        .dispatch(
            "comm.inbox",
            serde_json::json!({ "status": "all", "limit": 50 }),
        )
        .await
        .expect("anonymous inbox");
    let anon_count = anon_inbox["count"].as_u64().unwrap_or(0);
    assert_eq!(
        anon_count, 0,
        "#199 regression: anonymous inbox must NOT see messages addressed to lambda:b; \
         got count={anon_count}, inbox={anon_inbox}"
    );
}

/// #199 / single-tenant: anonymous caller still sees messages addressed to "local".
///
/// Party-line messages (to_actor="local" or to_actor absent) must remain visible
/// to anonymous callers — this is the OSS single-tenant case.
#[tokio::test]
async fn i199_anonymous_inbox_sees_local_messages() {
    // build_registry() has no actor_id → anonymous.
    let (registry, _rt) = build_registry();

    // Send to "local" — this is the single-tenant party-line path.
    registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "local", "content": "party-line msg" }),
        )
        .await
        .expect("self-send to local");

    let inbox = registry
        .dispatch("comm.inbox", serde_json::json!({ "status": "all" }))
        .await
        .expect("inbox");
    let count = inbox["count"].as_u64().unwrap_or(0);
    assert!(
        count >= 1,
        "#199 single-tenant: anonymous inbox must see messages addressed to 'local'; \
         got count={count}"
    );
}

// --- ingest routing tests (actor routing via default_inbound_actor + correlation) ---

/// Helper: ingest a message note and return the stored props.
async fn ingest_and_get_props(
    registry: &VerbRegistry,
    rt: &KhiveRuntime,
    params: serde_json::Value,
) -> serde_json::Value {
    let result = registry
        .dispatch("comm.ingest", params)
        .await
        .expect("ingest succeeds");
    assert!(
        !result
            .get("deduplicated")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        "message must not be deduplicated in routing tests"
    );
    let full_id = result["full_id"].as_str().expect("full_id present");
    let uuid = full_id.parse::<uuid::Uuid>().expect("valid UUID");
    let token = rt
        .authorize(khive_runtime::Namespace::local())
        .expect("authorize local");
    let store = rt.notes(&token).expect("notes store");
    let note = store
        .get_note(uuid)
        .await
        .expect("get_note ok")
        .expect("note exists");
    note.properties.expect("note has properties")
}

/// (a) Reply with correlation matching an outbound note whose from_actor=lambda:khive
/// → ingested note to_actor=lambda:khive.
#[tokio::test]
async fn ingest_routing_reply_routes_to_original_sender() {
    let (registry, rt) = build_registry_for_ns("local");

    // Plant an outbound message that looks like one lambda:khive sent.
    // We use comm.send to write the note, then read its external_id back.
    // We need a note with properties.external_id set so correlation resolution can find it.
    // Directly insert via the runtime store to control all fields.
    let outbound_external_id = "<sent-msg-001@khive.ai>";
    {
        use khive_storage::note::Note;
        let token = rt
            .authorize(khive_runtime::Namespace::local())
            .expect("authorize");
        let store = rt.notes(&token).expect("notes store");
        let now = chrono::Utc::now().timestamp_micros();
        let thread_uuid = uuid::Uuid::new_v4();
        let note = Note {
            id: uuid::Uuid::new_v4(),
            namespace: "local".into(),
            kind: "message".into(),
            status: "active".into(),
            name: None,
            content: "original outbound".into(),
            salience: None,
            decay_factor: None,
            expires_at: None,
            properties: Some(serde_json::json!({
                "direction": "outbound",
                "from": "email:mailbox@example.com",
                "to": "email:user@example.com",
                "from_actor": "lambda:khive",
                "to_actor": "email:user@example.com",
                "external_id": outbound_external_id,
                "thread_id": thread_uuid.as_hyphenated().to_string(),
                "sent_at": chrono::Utc::now().to_rfc3339(),
            })),
            created_at: now,
            updated_at: now,
            deleted_at: None,
        };
        store.upsert_note(note).await.expect("upsert outbound note");
    }

    // Ingest a reply whose correlation_external_id matches the outbound note's external_id.
    let props = ingest_and_get_props(
        &registry,
        &rt,
        serde_json::json!({
            "from": "email:user@example.com",
            "to": "email:mailbox@example.com",
            "content": "this is a reply",
            "correlation_external_id": outbound_external_id,
            "external_id": "imap:mail:1:1",
            "namespace": "local",
        }),
    )
    .await;

    assert_eq!(
        props["to_actor"].as_str(),
        Some("lambda:khive"),
        "reply must route to the original sender's actor; got props={props}"
    );
}

/// (a2) Regression: outbound stores its Message-ID in wire form `<id@domain>`, but an
/// inbound `In-Reply-To` is delivered bracket-free (`id@domain`) because `mail_parser`
/// strips the angle brackets. Pass-1 must still correlate the reply back to the original
/// sender. Before the bracket-toggle fix this fell through to `default_inbound_actor`
/// (lambda:leo) with a fresh thread — the exact failure seen on the live round-trip.
#[tokio::test]
async fn ingest_routing_reply_correlates_bracket_free_in_reply_to() {
    let (registry, rt) = build_registry_for_ns("local");

    // Outbound note stores the Message-ID WITH angle brackets (wire form).
    let outbound_external_id = "<sent-msg-002@khive.ai>";
    // Inbound In-Reply-To arrives WITHOUT brackets (mail_parser strips them).
    let inbound_correlation = "sent-msg-002@khive.ai";
    let thread_uuid = uuid::Uuid::new_v4().as_hyphenated().to_string();
    {
        use khive_storage::note::Note;
        let token = rt
            .authorize(khive_runtime::Namespace::local())
            .expect("authorize");
        let store = rt.notes(&token).expect("notes store");
        let now = chrono::Utc::now().timestamp_micros();
        let note = Note {
            id: uuid::Uuid::new_v4(),
            namespace: "local".into(),
            kind: "message".into(),
            status: "active".into(),
            name: None,
            content: "original outbound".into(),
            salience: None,
            decay_factor: None,
            expires_at: None,
            properties: Some(serde_json::json!({
                "direction": "outbound",
                "from": "email:mailbox@example.com",
                "to": "email:user@example.com",
                "from_actor": "lambda:khive",
                "to_actor": "email:user@example.com",
                "external_id": outbound_external_id,
                "thread_id": thread_uuid,
                "sent_at": chrono::Utc::now().to_rfc3339(),
            })),
            created_at: now,
            updated_at: now,
            deleted_at: None,
        };
        store.upsert_note(note).await.expect("upsert outbound note");
    }

    let props = ingest_and_get_props(
        &registry,
        &rt,
        serde_json::json!({
            "from": "email:user@example.com",
            "to": "email:mailbox@example.com",
            "content": "this is a bracket-free reply",
            "correlation_external_id": inbound_correlation,
            "external_id": "imap:mail:2:1",
            "default_inbound_actor": "lambda:leo",
            "namespace": "local",
        }),
    )
    .await;

    assert_eq!(
        props["to_actor"].as_str(),
        Some("lambda:khive"),
        "bracket-free In-Reply-To must correlate to the bracketed outbound external_id \
         and route to the original sender, not default_inbound_actor; got props={props}"
    );
    assert_eq!(
        props["thread_id"].as_str(),
        Some(thread_uuid.as_str()),
        "correlated reply must attach to the original thread, not a fresh root; \
         got props={props}"
    );
}

/// (b) Fresh message, no correlation, default_inbound_actor=lambda:leo → to_actor=lambda:leo.
#[tokio::test]
async fn ingest_routing_fresh_message_uses_default_actor() {
    let (registry, rt) = build_registry_for_ns("local");

    let props = ingest_and_get_props(
        &registry,
        &rt,
        serde_json::json!({
            "from": "email:stranger@example.com",
            "to": "email:mailbox@example.com",
            "content": "hello from a stranger",
            "external_id": "imap:mail:1:2",
            "default_inbound_actor": "lambda:leo",
            "namespace": "local",
        }),
    )
    .await;

    assert_eq!(
        props["to_actor"].as_str(),
        Some("lambda:leo"),
        "fresh message must route to default_inbound_actor; got props={props}"
    );
}

/// (c) No correlation, no default_inbound_actor → to_actor=p.to (back-compat).
#[tokio::test]
async fn ingest_routing_no_default_falls_back_to_to_field() {
    let (registry, rt) = build_registry_for_ns("local");

    let props = ingest_and_get_props(
        &registry,
        &rt,
        serde_json::json!({
            "from": "email:stranger@example.com",
            "to": "email:mailbox@example.com",
            "content": "back-compat message",
            "external_id": "imap:mail:1:3",
            "namespace": "local",
        }),
    )
    .await;

    assert_eq!(
        props["to_actor"].as_str(),
        Some("email:mailbox@example.com"),
        "no default actor: to_actor must fall back to p.to; got props={props}"
    );
}

// ── X-Khive-Thread-ID header correlation (thread-UUID fallback) ───
//
// When our own outbound email carries X-Khive-Thread-ID = <thread_uuid>, a reply
// that preserves that header arrives with correlation_external_id = <thread_uuid>.
// The existing pass-1 (external_id match) finds nothing because thread_uuid ≠
// the note's external_id (which is a Message-ID).  The new pass-2 matches
// $.thread_id on an outbound note to recover from_actor and route the reply
// back to the original sender's actor.

/// (d) Reply correlating via thread-UUID (X-Khive-Thread-ID fallback) routes to
/// the original sender's actor even when no external_id match exists.
#[tokio::test]
async fn ingest_routing_reply_via_thread_uuid_routes_to_original_sender() {
    use khive_storage::note::Note;

    let (registry, rt) = build_registry_for_ns("local");

    // Plant an outbound message note directly (simulates one we already sent).
    // Properties: external_id is a standard Message-ID; thread_id is the internal UUID.
    // from_actor is the actor we expect the reply to route back to.
    let thread_uuid = uuid::Uuid::new_v4().as_hyphenated().to_string();
    {
        let token = rt
            .authorize(khive_runtime::Namespace::local())
            .expect("authorize");
        let store = rt.notes(&token).expect("notes store");
        let now = chrono::Utc::now().timestamp_micros();
        let note = Note {
            id: uuid::Uuid::new_v4(),
            namespace: "local".into(),
            kind: "message".into(),
            status: "active".into(),
            name: None,
            content: "original outbound via thread".into(),
            salience: None,
            decay_factor: None,
            expires_at: None,
            properties: Some(serde_json::json!({
                "direction": "outbound",
                "from": "email:mailbox@example.com",
                "to": "email:user@example.com",
                "from_actor": "lambda:khive",
                "to_actor": "email:user@example.com",
                // external_id is a real Message-ID — NOT the thread_uuid.
                "external_id": "<original-message-id@khive.ai>",
                "thread_id": thread_uuid,
                "sent_at": chrono::Utc::now().to_rfc3339(),
            })),
            created_at: now,
            updated_at: now,
            deleted_at: None,
        };
        store.upsert_note(note).await.expect("upsert outbound note");
    }

    // Ingest a reply whose correlation_external_id is the thread_uuid (X-Khive-Thread-ID).
    // Pass-1 (external_id match) will find nothing because thread_uuid ≠ external_id.
    // Pass-2 (thread_id match on outbound) must recover from_actor=lambda:khive.
    let props = ingest_and_get_props(
        &registry,
        &rt,
        serde_json::json!({
            "from": "email:user@example.com",
            "to": "email:mailbox@example.com",
            "content": "this is a reply via X-Khive-Thread-ID",
            "correlation_external_id": thread_uuid,
            "external_id": "imap:mail:thread-uuid-reply:1",
            "namespace": "local",
        }),
    )
    .await;

    assert_eq!(
        props["to_actor"].as_str(),
        Some("lambda:khive"),
        "reply correlating via thread-UUID must route to lambda:khive \
         (original sender's actor); got props={props}"
    );
    assert_eq!(
        props["thread_id"].as_str(),
        Some(thread_uuid.as_str()),
        "ingested reply must be attached to the original thread_id; \
         got props={props}"
    );
}

// --- issue #403: In-Reply-To/References on outbound replies (native MUA threading) ---
//
// khive's own thread continuity uses X-Khive-Thread-ID / external_id correlation
// (tested above); native mail clients (iPhone Mail, Gmail) instead group
// conversations by RFC 5322 Message-ID ancestry, which these tests cover.

/// Helper: plant a message note directly with the given properties, returning its UUID.
async fn plant_message_note(
    rt: &KhiveRuntime,
    content: &str,
    props: serde_json::Value,
) -> uuid::Uuid {
    use khive_storage::note::Note;
    let token = rt
        .authorize(khive_runtime::Namespace::local())
        .expect("authorize");
    let store = rt.notes(&token).expect("notes store");
    let now = chrono::Utc::now().timestamp_micros();
    let id = uuid::Uuid::new_v4();
    let note = Note {
        id,
        namespace: "local".into(),
        kind: "message".into(),
        status: "active".into(),
        name: None,
        content: content.into(),
        salience: None,
        decay_factor: None,
        expires_at: None,
        properties: Some(props),
        created_at: now,
        updated_at: now,
        deleted_at: None,
    };
    store.upsert_note(note).await.expect("upsert planted note");
    id
}

/// Helper: dispatch `comm.reply` and return the newly created outbound note's properties.
async fn reply_and_get_outbound_props(
    registry: &VerbRegistry,
    rt: &KhiveRuntime,
    parent_id: uuid::Uuid,
    content: &str,
) -> serde_json::Value {
    let result = registry
        .dispatch(
            "comm.reply",
            serde_json::json!({
                "id": parent_id.as_hyphenated().to_string(),
                "content": content,
            }),
        )
        .await
        .expect("reply succeeds");
    let full_id = result["full_id"].as_str().expect("full_id present");
    let uuid = full_id.parse::<uuid::Uuid>().expect("valid UUID");
    let token = rt
        .authorize(khive_runtime::Namespace::local())
        .expect("authorize local");
    let store = rt.notes(&token).expect("notes store");
    let note = store
        .get_note(uuid)
        .await
        .expect("get_note ok")
        .expect("note exists");
    note.properties.expect("note has properties")
}

/// (a) Reply to an inbound-originated parent: the parent's Message-ID lives in
/// `wire_message_id` (bracket-free, as `mail_parser` delivers it), never in
/// `external_id`, which for an inbound note is the unrelated IMAP dedup key.
/// The reply must read `wire_message_id` and wrap it for the wire.
#[tokio::test]
async fn reply_sets_in_reply_to_for_inbound_originated_parent() {
    let (registry, rt) = build_registry_for_ns("local");

    let parent_id = plant_message_note(
        &rt,
        "hello from sender",
        serde_json::json!({
            "direction": "inbound",
            "from": "email:sender@example.com",
            "to": "email:mailbox@example.com",
            "from_actor": "email:sender@example.com",
            "to_actor": "lambda:khive",
            // IMAP dedup key -- must NOT be mistaken for a Message-ID.
            "external_id": "imap:host:1:42",
            // The email's own Message-ID, bracket-free as mail_parser delivers it.
            "wire_message_id": "inbound-msg-001@example.com",
            "thread_id": uuid::Uuid::new_v4().as_hyphenated().to_string(),
            "sent_at": chrono::Utc::now().to_rfc3339(),
        }),
    )
    .await;

    let props = reply_and_get_outbound_props(&registry, &rt, parent_id, "reply body").await;

    assert_eq!(
        props["in_reply_to_message_id"].as_str(),
        Some("<inbound-msg-001@example.com>"),
        "reply to an inbound-originated parent must set the bracket-wrapped \
         wire_message_id, not the unrelated IMAP-key external_id; got props={props}"
    );
}

/// (b) Reply to an outbound-minted parent: the parent's own Message-ID was
/// self-minted into `external_id` (bracketed) by the outbox delivery loop. The
/// reply must reuse it verbatim.
#[tokio::test]
async fn reply_sets_in_reply_to_for_outbound_minted_parent() {
    let (registry, rt) = build_registry_for_ns("local");

    let parent_id = plant_message_note(
        &rt,
        "our earlier note",
        serde_json::json!({
            "direction": "outbound",
            "from": "local",
            "to": "local",
            "from_actor": "lambda:khive",
            "to_actor": "email:sender@example.com",
            "external_id": "<outbound-msg-001@khive.ai>",
            "thread_id": uuid::Uuid::new_v4().as_hyphenated().to_string(),
            "sent_at": chrono::Utc::now().to_rfc3339(),
        }),
    )
    .await;

    let props = reply_and_get_outbound_props(&registry, &rt, parent_id, "reply body").await;

    assert_eq!(
        props["in_reply_to_message_id"].as_str(),
        Some("<outbound-msg-001@khive.ai>"),
        "reply to an outbound-minted parent must reuse its bracketed external_id \
         verbatim; got props={props}"
    );
}

/// (c) Reply to a parent with no known wire Message-ID (e.g. a khive-internal
/// message never routed through email): no In-Reply-To/References must be
/// fabricated, and the reply still succeeds exactly as before this feature.
#[tokio::test]
async fn reply_omits_in_reply_to_when_parent_has_no_wire_message_id() {
    let (registry, rt) = build_registry_for_ns("local");

    let parent_id = plant_message_note(
        &rt,
        "no wire id here",
        serde_json::json!({
            "direction": "inbound",
            "from": "lambda:leo",
            "to": "local",
            "from_actor": "lambda:leo",
            "to_actor": "lambda:khive",
            "thread_id": uuid::Uuid::new_v4().as_hyphenated().to_string(),
            "sent_at": chrono::Utc::now().to_rfc3339(),
        }),
    )
    .await;

    let props = reply_and_get_outbound_props(&registry, &rt, parent_id, "reply body").await;

    assert!(
        props.get("in_reply_to_message_id").is_none(),
        "reply to a parent without a wire Message-ID must not fabricate one; \
         got props={props}"
    );
}

/// `comm.ingest` with `wire_message_id` persists it on the resulting note, kept
/// distinct from `external_id` (the IMAP dedup key).
#[tokio::test]
async fn ingest_persists_wire_message_id_distinct_from_external_id() {
    let (registry, rt) = build_registry_for_ns("local");

    let props = ingest_and_get_props(
        &registry,
        &rt,
        serde_json::json!({
            "from": "email:sender@example.com",
            "to": "email:mailbox@example.com",
            "content": "hello",
            "external_id": "imap:host:1:99",
            "wire_message_id": "real-msg-id@example.com",
            "namespace": "local",
        }),
    )
    .await;

    assert_eq!(
        props["wire_message_id"].as_str(),
        Some("real-msg-id@example.com"),
        "comm.ingest must persist wire_message_id verbatim; got props={props}"
    );
    assert_eq!(
        props["external_id"].as_str(),
        Some("imap:host:1:99"),
        "wire_message_id must not overwrite the unrelated external_id dedup key; \
         got props={props}"
    );
}

/// `comm.ingest` without `wire_message_id` leaves it unset (no fabrication).
#[tokio::test]
async fn ingest_omits_wire_message_id_when_absent() {
    let (registry, rt) = build_registry_for_ns("local");

    let props = ingest_and_get_props(
        &registry,
        &rt,
        serde_json::json!({
            "from": "email:stranger@example.com",
            "to": "email:mailbox@example.com",
            "content": "hello",
            "external_id": "imap:host:1:100",
            "namespace": "local",
        }),
    )
    .await;

    assert!(
        props.get("wire_message_id").is_none(),
        "no wire_message_id in ingest params must mean none stored; got props={props}"
    );
}

/// `comm.ingest` with `wire_references` persists it on the resulting note, kept
/// distinct from `external_id` (the IMAP dedup key) and from `wire_message_id`.
#[tokio::test]
async fn ingest_persists_wire_references_distinct_from_external_id() {
    let (registry, rt) = build_registry_for_ns("local");

    let props = ingest_and_get_props(
        &registry,
        &rt,
        serde_json::json!({
            "from": "email:sender@example.com",
            "to": "email:mailbox@example.com",
            "content": "hello",
            "external_id": "imap:host:1:101",
            "wire_message_id": "real-msg-id@example.com",
            "wire_references": "<grandparent1@example.com> <parent123@example.com>",
            "namespace": "local",
        }),
    )
    .await;

    assert_eq!(
        props["wire_references"].as_str(),
        Some("<grandparent1@example.com> <parent123@example.com>"),
        "comm.ingest must persist wire_references verbatim; got props={props}"
    );
    assert_eq!(
        props["external_id"].as_str(),
        Some("imap:host:1:101"),
        "wire_references must not overwrite the unrelated external_id dedup key; \
         got props={props}"
    );
}

/// `comm.ingest` without `wire_references` leaves it unset (no fabrication).
#[tokio::test]
async fn ingest_omits_wire_references_when_absent() {
    let (registry, rt) = build_registry_for_ns("local");

    let props = ingest_and_get_props(
        &registry,
        &rt,
        serde_json::json!({
            "from": "email:stranger@example.com",
            "to": "email:mailbox@example.com",
            "content": "hello",
            "external_id": "imap:host:1:102",
            "namespace": "local",
        }),
    )
    .await;

    assert!(
        props.get("wire_references").is_none(),
        "no wire_references in ingest params must mean none stored; got props={props}"
    );
}

// --- issue #403: References must carry the full ancestor chain ---
//
// The prior implementation set References from the single
// `in_reply_to` value, dropping any ancestors before the immediate parent.
// These tests assert the exact serialized References/In-Reply-To values
// (not just presence) for each required case.

/// (a) Reply whose parent has an existing References chain of 2+ ids: the
/// reply's References must be that chain followed by the parent's own
/// Message-ID, and In-Reply-To must remain exactly the parent Message-ID.
#[tokio::test]
async fn reply_extends_existing_references_chain_of_two_or_more() {
    let (registry, rt) = build_registry_for_ns("local");

    let parent_id = plant_message_note(
        &rt,
        "hello from sender",
        serde_json::json!({
            "direction": "inbound",
            "from": "email:sender@example.com",
            "to": "email:mailbox@example.com",
            "from_actor": "email:sender@example.com",
            "to_actor": "lambda:khive",
            "external_id": "imap:host:1:43",
            "wire_message_id": "parent123@example.com",
            "wire_references": "grandparent1@example.com grandparent2@example.com",
            "thread_id": uuid::Uuid::new_v4().as_hyphenated().to_string(),
            "sent_at": chrono::Utc::now().to_rfc3339(),
        }),
    )
    .await;

    let props = reply_and_get_outbound_props(&registry, &rt, parent_id, "reply body").await;

    assert_eq!(
        props["in_reply_to_message_id"].as_str(),
        Some("<parent123@example.com>"),
        "In-Reply-To must be exactly the parent Message-ID; got props={props}"
    );
    assert_eq!(
        props["references_chain"].as_str(),
        Some("<grandparent1@example.com> <grandparent2@example.com> <parent123@example.com>"),
        "References must carry the parent's full existing chain followed by its own \
         Message-ID, not just the immediate parent; got props={props}"
    );
}

/// (b) Reply whose parent has no References chain of its own (e.g. it was a
/// thread root): References must degrade gracefully to the parent Message-ID
/// alone, identical to pre-chain-preservation behavior.
#[tokio::test]
async fn reply_references_falls_back_to_parent_message_id_when_no_chain() {
    let (registry, rt) = build_registry_for_ns("local");

    let parent_id = plant_message_note(
        &rt,
        "hello from sender",
        serde_json::json!({
            "direction": "inbound",
            "from": "email:sender@example.com",
            "to": "email:mailbox@example.com",
            "from_actor": "email:sender@example.com",
            "to_actor": "lambda:khive",
            "external_id": "imap:host:1:44",
            "wire_message_id": "parent456@example.com",
            "thread_id": uuid::Uuid::new_v4().as_hyphenated().to_string(),
            "sent_at": chrono::Utc::now().to_rfc3339(),
        }),
    )
    .await;

    let props = reply_and_get_outbound_props(&registry, &rt, parent_id, "reply body").await;

    assert_eq!(
        props["in_reply_to_message_id"].as_str(),
        Some("<parent456@example.com>")
    );
    assert_eq!(
        props["references_chain"].as_str(),
        Some("<parent456@example.com>"),
        "no stored chain on the parent must mean References = parent Message-ID alone; \
         got props={props}"
    );
}

/// (c) Reply-to-outbound direction: the parent was one of our own prior sends,
/// so its chain lives in `references_chain` (not `wire_references`). A reply
/// must extend THAT chain, proving the direction-aware read is wired through
/// `comm.reply` end-to-end, not just unit-tested on `parent_references_chain`.
#[tokio::test]
async fn reply_extends_references_chain_for_outbound_parent() {
    let (registry, rt) = build_registry_for_ns("local");

    let parent_id = plant_message_note(
        &rt,
        "our earlier reply",
        serde_json::json!({
            "direction": "outbound",
            "from": "local",
            "to": "local",
            "from_actor": "lambda:khive",
            "to_actor": "email:sender@example.com",
            "external_id": "<outbound-msg-002@khive.ai>",
            // Realistic stored shape: an outbound row's own `references_chain` is
            // ancestors-only (exactly what `build_references_header` computes for
            // it when it was sent) and never contains that same row's own
            // `external_id`. See the dedicated dedup regression below for the
            // tainted-data case where a stored chain does contain it.
            "references_chain": "<root1@example.com>",
            "thread_id": uuid::Uuid::new_v4().as_hyphenated().to_string(),
            "sent_at": chrono::Utc::now().to_rfc3339(),
        }),
    )
    .await;

    let props = reply_and_get_outbound_props(&registry, &rt, parent_id, "reply body").await;

    assert_eq!(
        props["in_reply_to_message_id"].as_str(),
        Some("<outbound-msg-002@khive.ai>"),
        "In-Reply-To must be exactly the outbound parent's self-minted external_id; \
         got props={props}"
    );
    assert_eq!(
        props["references_chain"].as_str(),
        Some("<root1@example.com> <outbound-msg-002@khive.ai>"),
        "reply-to-outbound must extend the outbound parent's own references_chain \
         (read direction-aware, not wire_references) followed by its Message-ID; \
         got props={props}"
    );
}

/// (d) A malformed token embedded in the parent's stored chain must be skipped
/// rather than propagated into the reply's References header.
#[tokio::test]
async fn reply_skips_malformed_token_in_parent_references_chain() {
    let (registry, rt) = build_registry_for_ns("local");

    let parent_id = plant_message_note(
        &rt,
        "hello from sender",
        serde_json::json!({
            "direction": "inbound",
            "from": "email:sender@example.com",
            "to": "email:mailbox@example.com",
            "from_actor": "email:sender@example.com",
            "to_actor": "lambda:khive",
            "external_id": "imap:host:1:45",
            "wire_message_id": "parent789@example.com",
            "wire_references": "good1@example.com not-a-message-id good2@example.com",
            "thread_id": uuid::Uuid::new_v4().as_hyphenated().to_string(),
            "sent_at": chrono::Utc::now().to_rfc3339(),
        }),
    )
    .await;

    let props = reply_and_get_outbound_props(&registry, &rt, parent_id, "reply body").await;

    assert_eq!(
        props["references_chain"].as_str(),
        Some("<good1@example.com> <good2@example.com> <parent789@example.com>"),
        "a malformed token in the parent's stored chain must be skipped, not \
         propagated into the reply's References; got props={props}"
    );
}

/// (e) A stored `references_chain` that is itself tainted -- already containing
/// an equivalent of the parent's own Message-ID (e.g. legacy/corrupted data;
/// this exact shape is never produced by `comm.reply` itself, see test (c)
/// above, which now uses the realistic ancestors-only shape) -- must not be
/// propagated as a literal duplicate. The duplicate is dropped and first-seen
/// order is preserved: the parent's id keeps its original position in the
/// chain rather than being appended again at the end.
#[tokio::test]
async fn reply_dedups_tainted_parent_references_chain_containing_parent_id() {
    let (registry, rt) = build_registry_for_ns("local");

    let parent_id = plant_message_note(
        &rt,
        "our earlier reply",
        serde_json::json!({
            "direction": "outbound",
            "from": "local",
            "to": "local",
            "from_actor": "lambda:khive",
            "to_actor": "email:sender@example.com",
            "external_id": "<dup-msg@khive.ai>",
            "references_chain": "<root1@example.com> <dup-msg@khive.ai> <root2@example.com>",
            "thread_id": uuid::Uuid::new_v4().as_hyphenated().to_string(),
            "sent_at": chrono::Utc::now().to_rfc3339(),
        }),
    )
    .await;

    let props = reply_and_get_outbound_props(&registry, &rt, parent_id, "reply body").await;

    assert_eq!(
        props["in_reply_to_message_id"].as_str(),
        Some("<dup-msg@khive.ai>")
    );
    assert_eq!(
        props["references_chain"].as_str(),
        Some("<root1@example.com> <dup-msg@khive.ai> <root2@example.com>"),
        "a tainted chain already containing the parent's own id must be \
         deduplicated (not doubled at the end) and keep first-seen order; \
         got props={props}"
    );
}

// --- issue #448: quarantine metadata must survive comm.ingest persistence ---

/// A quarantined envelope (as `EmailChannel::quarantine_envelope` builds it, ADR-056
/// Amendment 2026-07-02) must persist its `quarantined`/`quarantine_reason`/
/// `quarantine_claimed_from` markers through `comm.ingest`, and `from`/`from_actor`
/// must stay the fixed `email:quarantine` marker -- `quarantine_claimed_from` is
/// carried for maintainer review only, never as an attribution source.
#[tokio::test]
async fn ingest_persists_quarantine_metadata_and_never_attributes_claimed_sender() {
    let (registry, rt) = build_registry_for_ns("local");

    let props = ingest_and_get_props(
        &registry,
        &rt,
        serde_json::json!({
            "from": "email:quarantine",
            "to": "email:maintainer@example.com",
            "content": "spoofed body",
            "subject": "spoofed, no auth at all",
            "channel_kind": "email",
            "external_id": "imap:mail:1:1",
            "namespace": "local",
            "metadata": {
                "quarantined": "true",
                "quarantine_reason": "auth-absent",
                "quarantine_claimed_from": "maintainer@example.com",
            },
        }),
    )
    .await;

    assert_eq!(
        props["quarantined"].as_str(),
        Some("true"),
        "quarantine marker must reach persisted properties; got props={props}"
    );
    assert_eq!(
        props["quarantine_reason"].as_str(),
        Some("auth-absent"),
        "quarantine reason must reach persisted properties; got props={props}"
    );
    assert_eq!(
        props["quarantine_claimed_from"].as_str(),
        Some("maintainer@example.com"),
        "the claimed From is preserved in metadata for maintainer review; got props={props}"
    );
    assert_eq!(
        props["from"].as_str(),
        Some("email:quarantine"),
        "quarantine_claimed_from must never be used as an authoritative sender: \
         `from` must stay the fixed quarantine marker"
    );
    assert_eq!(
        props["from_actor"].as_str(),
        Some("email:quarantine"),
        "quarantine_claimed_from must never be used as an authoritative sender: \
         `from_actor` must stay the fixed quarantine marker"
    );
}

/// Absent `metadata` must leave persisted properties exactly as before this fix
/// (no `quarantined`/`quarantine_reason`/`quarantine_claimed_from` keys at all).
#[tokio::test]
async fn ingest_without_metadata_persists_no_quarantine_keys() {
    let (registry, rt) = build_registry_for_ns("local");

    let props = ingest_and_get_props(
        &registry,
        &rt,
        serde_json::json!({
            "from": "email:user@example.com",
            "to": "email:mailbox@example.com",
            "content": "ordinary message",
            "external_id": "imap:mail:2:1",
            "namespace": "local",
        }),
    )
    .await;

    assert!(
        props.get("quarantined").is_none(),
        "absent metadata must not fabricate a quarantined key; got props={props}"
    );
    assert!(
        props.get("quarantine_reason").is_none(),
        "absent metadata must not fabricate a quarantine_reason key; got props={props}"
    );
    assert!(
        props.get("quarantine_claimed_from").is_none(),
        "absent metadata must not fabricate a quarantine_claimed_from key; got props={props}"
    );
}

/// Metadata must merge additively: it must never be able to override an
/// identity/routing field the handler already stamped (from, from_actor,
/// to_actor, direction). This is the safety property that makes the generic
/// passthrough non-leaky even though the comm pack does not special-case any key.
#[tokio::test]
async fn ingest_metadata_cannot_override_stamped_identity_fields() {
    let (registry, rt) = build_registry_for_ns("local");

    let props = ingest_and_get_props(
        &registry,
        &rt,
        serde_json::json!({
            "from": "email:quarantine",
            "to": "email:maintainer@example.com",
            "content": "spoofed body",
            "external_id": "imap:mail:3:1",
            "namespace": "local",
            "metadata": {
                "from_actor": "lambda:leo",
                "to_actor": "lambda:leo",
                "direction": "outbound",
            },
        }),
    )
    .await;

    assert_eq!(
        props["from_actor"].as_str(),
        Some("email:quarantine"),
        "metadata must never override the handler-stamped from_actor; got props={props}"
    );
    assert_eq!(
        props["direction"].as_str(),
        Some("inbound"),
        "metadata must never override the handler-stamped direction; got props={props}"
    );
}

// ── Issue #479a: comm.ingest must reject malformed thread_id ─────────────────

/// `comm.ingest` with a malformed `thread_id` must return `InvalidInput` and
/// must not write any note (issue #479a). Before the fix, an invalid thread_id
/// was silently filtered out and replaced with a fresh UUID, splitting the
/// message into the wrong conversation while still reporting success.
#[tokio::test]
async fn ingest_rejects_malformed_thread_id_without_writing_note() {
    let (registry, rt) = build_registry_for_ns("local");

    let result = registry
        .dispatch(
            "comm.ingest",
            serde_json::json!({
                "thread_id": "not-a-uuid",
                "from": "email:a@example.com",
                "to": "email:b@example.com",
                "content": "reply",
                "namespace": "local",
            }),
        )
        .await;

    let err = result.expect_err("ingest with malformed thread_id must fail");
    let err_msg = err.to_string();
    assert!(
        matches!(err, khive_runtime::RuntimeError::InvalidInput(_)),
        "expected InvalidInput, got: {err:?}"
    );
    assert!(
        err_msg.contains("thread_id"),
        "error must mention thread_id; got: {err_msg}"
    );

    let token = rt
        .authorize(khive_runtime::Namespace::parse("local").unwrap())
        .expect("authorize local");
    let notes = rt
        .list_notes(&token, Some("message"), 100, 0)
        .await
        .expect("list_notes");
    let alive = notes.iter().filter(|n| n.deleted_at.is_none()).count();
    assert_eq!(
        alive, 0,
        "no note may be written when thread_id validation fails; got {alive}"
    );
}

/// `comm.ingest` with a valid UUID `thread_id` must succeed and persist it verbatim.
#[tokio::test]
async fn ingest_accepts_valid_uuid_thread_id() {
    let (registry, rt) = build_registry_for_ns("local");

    let supplied_thread_id = uuid::Uuid::new_v4().as_hyphenated().to_string();
    let result = registry
        .dispatch(
            "comm.ingest",
            serde_json::json!({
                "thread_id": supplied_thread_id,
                "from": "email:a@example.com",
                "to": "email:b@example.com",
                "content": "reply",
                "namespace": "local",
            }),
        )
        .await
        .expect("ingest with valid UUID thread_id must succeed");

    assert_eq!(
        result["thread_id"].as_str(),
        Some(supplied_thread_id.as_str()),
        "response thread_id must equal the supplied UUID; got {result}"
    );

    let full_id = result["full_id"].as_str().expect("full_id present");
    let uuid = full_id.parse::<uuid::Uuid>().expect("valid UUID");
    let token = rt
        .authorize(khive_runtime::Namespace::parse("local").unwrap())
        .expect("authorize local");
    let store = rt.notes(&token).expect("notes store");
    let note = store
        .get_note(uuid)
        .await
        .expect("get_note ok")
        .expect("note exists");
    assert_eq!(
        note.properties
            .as_ref()
            .and_then(|p| p.get("thread_id"))
            .and_then(|v| v.as_str()),
        Some(supplied_thread_id.as_str()),
        "stored note properties.thread_id must equal the supplied UUID"
    );
}

// ── Issue #479b: missing thread roots fall back to the matched message's own UUID ──

/// A reply correlated to an outbound message that has no `thread_id` property
/// (e.g. a legacy/imported row) must reuse the outbound note's own UUID as the
/// canonical root and route to the original `from_actor`, instead of being
/// treated as unmatched and split into a fresh thread routed to the default
/// inbound actor.
#[tokio::test]
async fn ingest_correlation_without_thread_id_uses_matched_message_id_as_root() {
    let (registry, rt) = build_registry_for_ns("local");

    let outbound_external_id = "<legacy@khive.ai>";
    let outbound_id = uuid::Uuid::new_v4();
    {
        use khive_storage::note::Note;
        let token = rt
            .authorize(khive_runtime::Namespace::local())
            .expect("authorize");
        let store = rt.notes(&token).expect("notes store");
        let now = chrono::Utc::now().timestamp_micros();
        let note = Note {
            id: outbound_id,
            namespace: "local".into(),
            kind: "message".into(),
            status: "active".into(),
            name: None,
            content: "legacy outbound".into(),
            salience: None,
            decay_factor: None,
            expires_at: None,
            properties: Some(serde_json::json!({
                "direction": "outbound",
                "from": "email:mailbox@example.com",
                "to": "email:user@example.com",
                "from_actor": "lambda:khive",
                "to_actor": "email:user@example.com",
                "external_id": outbound_external_id,
                // No `thread_id` -- simulates a legacy/imported root row written
                // before the canonical thread_id field existed.
                "sent_at": chrono::Utc::now().to_rfc3339(),
            })),
            created_at: now,
            updated_at: now,
            deleted_at: None,
        };
        store.upsert_note(note).await.expect("upsert outbound note");
    }

    let result = registry
        .dispatch(
            "comm.ingest",
            serde_json::json!({
                "from": "email:user@example.com",
                "to": "email:mailbox@example.com",
                "content": "reply to legacy root",
                "correlation_external_id": outbound_external_id,
                "external_id": "imap:mail:legacy:1",
                "default_inbound_actor": "lambda:leo",
                "namespace": "local",
            }),
        )
        .await
        .expect("ingest succeeds");

    let expected_thread_id = outbound_id.as_hyphenated().to_string();
    assert_eq!(
        result["thread_id"].as_str(),
        Some(expected_thread_id.as_str()),
        "reply must use the matched outbound note's own UUID as the canonical root; got {result}"
    );

    let full_id = result["full_id"].as_str().expect("full_id present");
    let uuid = full_id.parse::<uuid::Uuid>().expect("valid UUID");
    let token = rt
        .authorize(khive_runtime::Namespace::local())
        .expect("authorize local");
    let store = rt.notes(&token).expect("notes store");
    let note = store
        .get_note(uuid)
        .await
        .expect("get_note ok")
        .expect("note exists");
    let props = note.properties.expect("note has properties");
    assert_eq!(
        props["thread_id"].as_str(),
        Some(expected_thread_id.as_str()),
        "stored reply properties.thread_id must equal the outbound note's own UUID"
    );
    assert_eq!(
        props["to_actor"].as_str(),
        Some("lambda:khive"),
        "reply must route to the original from_actor, not default_inbound_actor; got props={props}"
    );
}

/// `comm.thread` must include a root message that has no `thread_id` property
/// at all (issue #479b) -- the SQL query only matches `properties.thread_id ==
/// root`, which a thread-id-less root can never satisfy on its own.
#[tokio::test]
async fn thread_includes_root_message_without_thread_id_property() {
    let (registry, rt) = build_registry_for_ns("local");

    let root_id = uuid::Uuid::new_v4();
    {
        use khive_storage::note::Note;
        let token = rt
            .authorize(khive_runtime::Namespace::local())
            .expect("authorize");
        let store = rt.notes(&token).expect("notes store");
        let now = chrono::Utc::now().timestamp_micros();
        let root_note = Note {
            id: root_id,
            namespace: "local".into(),
            kind: "message".into(),
            status: "active".into(),
            name: None,
            content: "legacy root, no thread_id".into(),
            salience: None,
            decay_factor: None,
            expires_at: None,
            properties: Some(serde_json::json!({
                "direction": "outbound",
                "from": "local",
                "to": "local",
                "sent_at": chrono::Utc::now().to_rfc3339(),
            })),
            created_at: now,
            updated_at: now,
            deleted_at: None,
        };
        store.upsert_note(root_note).await.expect("upsert root");

        let child_note = Note {
            id: uuid::Uuid::new_v4(),
            namespace: "local".into(),
            kind: "message".into(),
            status: "active".into(),
            name: None,
            content: "child reply".into(),
            salience: None,
            decay_factor: None,
            expires_at: None,
            properties: Some(serde_json::json!({
                "direction": "inbound",
                "from": "local",
                "to": "local",
                "thread_id": root_id.as_hyphenated().to_string(),
                "sent_at": chrono::Utc::now().to_rfc3339(),
            })),
            created_at: now + 1,
            updated_at: now + 1,
            deleted_at: None,
        };
        store.upsert_note(child_note).await.expect("upsert child");
    }

    let thread_result = registry
        .dispatch(
            "comm.thread",
            serde_json::json!({ "id": root_id.as_hyphenated().to_string() }),
        )
        .await
        .expect("thread dispatch succeeds");

    assert_eq!(
        thread_result["thread_id"].as_str(),
        Some(root_id.as_hyphenated().to_string().as_str()),
        "canonical thread_id must be the root's own UUID; got {thread_result}"
    );
    let messages = thread_result["messages"]
        .as_array()
        .expect("messages is an array");
    let root_full_id = root_id.as_hyphenated().to_string();
    assert!(
        messages
            .iter()
            .any(|m| m.get("full_id").and_then(|v| v.as_str()) == Some(root_full_id.as_str())),
        "thread must include the root message even though it has no thread_id property; got {thread_result}"
    );
    let count = thread_result["count"].as_u64().expect("count present");
    assert!(
        count >= 2,
        "thread must include root + child (at least 2); got count={count}"
    );
}

// --- comm.heartbeat / comm.health (khive #606) ---

/// design review amendment 1 (blocking): a fresh install with no persisted daemon
/// heartbeat state must report `role: "client"` with an empty channel list —
/// never fabricate channel entries the comm pack has no evidence for.
#[tokio::test]
async fn health_reports_client_role_when_no_heartbeat_state_exists() {
    let (registry, _rt) = build_registry();

    let result = registry
        .dispatch("comm.health", serde_json::json!({}))
        .await
        .expect("health succeeds");

    assert_eq!(result["role"].as_str(), Some("client"));
    assert!(result["source"].is_null());
    assert!(result["as_of"].as_str().is_some());
    assert_eq!(
        result["channels"]
            .as_array()
            .expect("channels is array")
            .len(),
        0
    );
}

/// ADR-103 Stage 1 / issue #723 ask 2: `comm.health()` must self-report this
/// process's own resource usage — `cpu_us`/`rss_bytes` via `getrusage`, plus
/// the (possibly empty) set of named background phases currently in flight.
/// No computed `healthy` field, matching the rest of this verb's contract.
#[tokio::test]
async fn health_includes_resource_self_report() {
    let (registry, _rt) = build_registry();

    let result = registry
        .dispatch("comm.health", serde_json::json!({}))
        .await
        .expect("health succeeds");

    let resource = &result["resource"];
    assert!(
        resource.is_object(),
        "resource must be an object, got: {resource:?}"
    );
    assert!(
        resource.get("healthy").is_none(),
        "resource must never carry a computed healthy bool"
    );
    // `getrusage` should succeed on every CI runner this crate builds on
    // (unix); the field must at least be present (null only on a platform
    // with no implementation) and non-negative when populated.
    assert!(
        resource.get("cpu_us").is_some(),
        "cpu_us key must be present"
    );
    if let Some(cpu_us) = resource["cpu_us"].as_i64() {
        assert!(cpu_us >= 0);
    }
    assert!(
        resource.get("rss_bytes").is_some(),
        "rss_bytes key must be present"
    );
    let active_phases = resource["active_phases"]
        .as_array()
        .expect("active_phases must be an array");
    assert!(
        active_phases.is_empty(),
        "no background phase is in flight during this test"
    );
}

/// comm.health() takes no arguments — any caller-supplied args must be rejected
/// rather than silently ignored (spec: "read-only, NO args").
#[tokio::test]
async fn health_rejects_stray_args() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch("comm.health", serde_json::json!({ "limit": 10 }))
        .await
        .expect_err("health must reject unexpected args");
    assert!(
        err.to_string().contains("takes no arguments"),
        "unexpected error message: {err}"
    );
}

/// Core cross-process-read contract (design review amendment 1): once the daemon
/// persists a successful heartbeat, `comm.health()` returns it annotated
/// `role: "daemon"`, `source: "daemon-heartbeat"` — this is true even though
/// the *reading* call is a plain in-process dispatch here, mirroring a
/// client-role stdio caller reading state it did not write itself.
#[tokio::test]
async fn heartbeat_success_is_visible_via_health() {
    let (registry, _rt) = build_registry_for_ns("local");

    registry
        .dispatch(
            "comm.heartbeat",
            serde_json::json!({
                "namespace": "local",
                "channel_kind": "email",
                "channel_slug": "recipient@example.com",
                "outcome": "success",
            }),
        )
        .await
        .expect("heartbeat succeeds");

    let health = registry
        .dispatch("comm.health", serde_json::json!({}))
        .await
        .expect("health succeeds");

    assert_eq!(health["role"].as_str(), Some("daemon"));
    assert_eq!(health["source"].as_str(), Some("daemon-heartbeat"));
    let channels = health["channels"].as_array().expect("channels is array");
    assert_eq!(channels.len(), 1);
    let ch = &channels[0];
    assert_eq!(ch["channel_kind"].as_str(), Some("email"));
    assert_eq!(ch["channel_slug"].as_str(), Some("recipient@example.com"));
    assert!(ch["last_success_at"].as_str().is_some());
    assert!(ch["last_poll_attempt_at"].as_str().is_some());
    assert!(ch["last_failure_at"].is_null());
    assert!(ch["last_error"].is_null());
    assert_eq!(ch["consecutive_failures"].as_u64(), Some(0));
}

/// design review amendment 3: `last_error` is RETAINED after a subsequent success
/// (callers compare `last_error.at` vs `last_success_at`), and
/// `consecutive_failures` resets to 0 on success.
#[tokio::test]
async fn heartbeat_retains_last_error_after_success_but_resets_consecutive_failures() {
    let (registry, _rt) = build_registry_for_ns("local");

    registry
        .dispatch(
            "comm.heartbeat",
            serde_json::json!({
                "namespace": "local",
                "channel_kind": "email",
                "channel_slug": "recipient@example.com",
                "outcome": "failure",
                "error_class": "auth",
                "error_message": "XOAUTH2 handshake failed",
            }),
        )
        .await
        .expect("first failure heartbeat succeeds");
    registry
        .dispatch(
            "comm.heartbeat",
            serde_json::json!({
                "namespace": "local",
                "channel_kind": "email",
                "channel_slug": "recipient@example.com",
                "outcome": "failure",
                "error_class": "auth",
                "error_message": "XOAUTH2 handshake failed",
            }),
        )
        .await
        .expect("second failure heartbeat succeeds");

    let after_failures = registry
        .dispatch("comm.health", serde_json::json!({}))
        .await
        .expect("health succeeds");
    let ch = &after_failures["channels"][0];
    assert_eq!(ch["consecutive_failures"].as_u64(), Some(2));
    assert_eq!(ch["last_error"]["class"].as_str(), Some("auth"));

    registry
        .dispatch(
            "comm.heartbeat",
            serde_json::json!({
                "namespace": "local",
                "channel_kind": "email",
                "channel_slug": "recipient@example.com",
                "outcome": "success",
            }),
        )
        .await
        .expect("success heartbeat succeeds");

    let after_success = registry
        .dispatch("comm.health", serde_json::json!({}))
        .await
        .expect("health succeeds");
    let ch = &after_success["channels"][0];
    assert_eq!(
        ch["consecutive_failures"].as_u64(),
        Some(0),
        "consecutive_failures must reset to 0 on success"
    );
    assert_eq!(
        ch["last_error"]["class"].as_str(),
        Some("auth"),
        "last_error must be RETAINED after a subsequent success (design review amendment 3)"
    );
    assert!(
        ch["last_success_at"].as_str().is_some(),
        "last_success_at must be set"
    );
}

/// design review amendment 2: rows are keyed by channel slug + kind, never kind
/// alone — two accounts of the same kind must not collapse into one row.
#[tokio::test]
async fn heartbeat_keys_by_slug_not_kind_alone() {
    let (registry, _rt) = build_registry_for_ns("local");

    registry
        .dispatch(
            "comm.heartbeat",
            serde_json::json!({
                "namespace": "local",
                "channel_kind": "email",
                "channel_slug": "recipient@example.com",
                "outcome": "success",
            }),
        )
        .await
        .expect("first account heartbeat succeeds");
    registry
        .dispatch(
            "comm.heartbeat",
            serde_json::json!({
                "namespace": "local",
                "channel_kind": "email",
                "channel_slug": "ops@khive.ai",
                "outcome": "failure",
                "error_class": "transport",
                "error_message": "connect timeout",
            }),
        )
        .await
        .expect("second account heartbeat succeeds");

    let health = registry
        .dispatch("comm.health", serde_json::json!({}))
        .await
        .expect("health succeeds");
    let channels = health["channels"].as_array().expect("channels is array");
    assert_eq!(
        channels.len(),
        2,
        "two accounts of the same channel_kind must produce two distinct rows; got {channels:?}"
    );
    let slugs: std::collections::HashSet<&str> = channels
        .iter()
        .map(|c| c["channel_slug"].as_str().unwrap())
        .collect();
    assert!(slugs.contains("recipient@example.com"));
    assert!(slugs.contains("ops@khive.ai"));
}

/// Repeated heartbeats for the same (kind, slug) update the same row (via
/// `upsert_note`'s deterministic id) rather than accumulating duplicates.
#[tokio::test]
async fn heartbeat_repeated_calls_update_same_row() {
    let (registry, _rt) = build_registry_for_ns("local");

    for _ in 0..3 {
        registry
            .dispatch(
                "comm.heartbeat",
                serde_json::json!({
                    "namespace": "local",
                    "channel_kind": "email",
                    "channel_slug": "recipient@example.com",
                    "outcome": "success",
                }),
            )
            .await
            .expect("heartbeat succeeds");
    }

    let health = registry
        .dispatch("comm.health", serde_json::json!({}))
        .await
        .expect("health succeeds");
    assert_eq!(
        health["channels"].as_array().expect("array").len(),
        1,
        "repeated heartbeats for the same channel must update one row, not accumulate"
    );
}

#[tokio::test]
async fn heartbeat_requires_error_class_on_failure() {
    let (registry, _rt) = build_registry_for_ns("local");

    let err = registry
        .dispatch(
            "comm.heartbeat",
            serde_json::json!({
                "namespace": "local",
                "channel_kind": "email",
                "channel_slug": "recipient@example.com",
                "outcome": "failure",
            }),
        )
        .await
        .expect_err("failure outcome without error_class must be rejected");
    assert!(
        err.to_string().contains("error_class"),
        "unexpected error message: {err}"
    );
}

#[tokio::test]
async fn heartbeat_rejects_invalid_outcome() {
    let (registry, _rt) = build_registry_for_ns("local");

    let err = registry
        .dispatch(
            "comm.heartbeat",
            serde_json::json!({
                "namespace": "local",
                "channel_kind": "email",
                "channel_slug": "recipient@example.com",
                "outcome": "maybe",
            }),
        )
        .await
        .expect_err("invalid outcome must be rejected");
    assert!(
        err.to_string().contains("outcome"),
        "unexpected error message: {err}"
    );
}

/// Spec: "report TIMESTAMPS only ... never a computed `healthy: bool`" —
/// the channel entry shape must not carry any boolean health verdict.
#[tokio::test]
async fn health_channel_entry_never_carries_a_healthy_bool() {
    let (registry, _rt) = build_registry_for_ns("local");

    registry
        .dispatch(
            "comm.heartbeat",
            serde_json::json!({
                "namespace": "local",
                "channel_kind": "email",
                "channel_slug": "recipient@example.com",
                "outcome": "failure",
                "error_class": "auth",
                "error_message": "handshake failed",
            }),
        )
        .await
        .expect("heartbeat succeeds");

    let health = registry
        .dispatch("comm.health", serde_json::json!({}))
        .await
        .expect("health succeeds");
    let ch = health["channels"][0]
        .as_object()
        .expect("channel entry is an object");
    assert!(
        !ch.contains_key("healthy"),
        "channel entry must never carry a computed healthy bool: {ch:?}"
    );
}

/// khive #877: `comm.health` must read `channel_health` rows from the
/// caller's injected namespace (`token.namespace()`), not the fixed
/// `khive_pack_comm::CHANNEL_HEALTH_NAMESPACE` constant. Plants one row
/// directly under `"local"` and one directly under a non-local `"tenant-a"`
/// namespace (bypassing `comm.heartbeat`, which still always writes to
/// `"local"` — this test exercises the read path only). An unscoped call
/// defaults to `"local"` and must see only the local row; a call with an
/// explicit `namespace="tenant-a"` must see only tenant-a's row, never
/// local's. Also asserts the response's `namespace` field (khive #877)
/// names the namespace actually read for both the unscoped and the
/// explicitly-scoped call, so a caller can tell "no daemon anywhere" apart
/// from "no rows under my scope yet" instead of the two cases being
/// indistinguishable client-role/empty-channels responses.
#[tokio::test]
async fn health_scoped_to_injected_namespace_sees_only_its_own_rows() {
    use khive_storage::note::Note;

    let (registry, rt) = build_registry_for_ns("local");

    let plant = |ns: &'static str, slug: &'static str| {
        let rt = rt.clone();
        async move {
            let token = rt
                .authorize(khive_runtime::Namespace::parse(ns).expect("valid namespace"))
                .expect("authorize namespace");
            let store = rt.notes(&token).expect("notes store");
            let now = chrono::Utc::now().timestamp_micros();
            let note = Note {
                id: uuid::Uuid::new_v4(),
                namespace: ns.to_string(),
                kind: "channel_health".to_string(),
                status: "active".to_string(),
                name: Some(format!("email:{slug}")),
                content: format!("channel heartbeat: email:{slug}"),
                salience: None,
                decay_factor: None,
                expires_at: None,
                properties: Some(serde_json::json!({
                    "channel_kind": "email",
                    "channel_slug": slug,
                    "last_success_at": chrono::Utc::now().to_rfc3339(),
                    "last_poll_attempt_at": chrono::Utc::now().to_rfc3339(),
                    "last_failure_at": null,
                    "last_error": null,
                    "consecutive_failures": 0,
                })),
                created_at: now,
                updated_at: now,
                deleted_at: None,
            };
            store
                .upsert_note(note)
                .await
                .expect("upsert channel_health note");
        }
    };
    plant("local", "local-inbox@example.com").await;
    plant("tenant-a", "tenant-a-inbox@example.com").await;

    let default_health = registry
        .dispatch("comm.health", serde_json::json!({}))
        .await
        .expect("unscoped health succeeds");
    let default_channels = default_health["channels"].as_array().expect("array");
    assert_eq!(
        default_channels.len(),
        1,
        "unscoped comm.health must default to the local namespace: {default_channels:?}"
    );
    assert_eq!(
        default_channels[0]["channel_slug"].as_str(),
        Some("local-inbox@example.com")
    );
    assert_eq!(
        default_health["namespace"].as_str(),
        Some("local"),
        "response must echo the namespace actually read, defaulting to local: {default_health}"
    );

    let scoped_health = registry
        .dispatch(
            "comm.health",
            serde_json::json!({ "namespace": "tenant-a" }),
        )
        .await
        .expect("namespace-scoped health succeeds");
    let scoped_channels = scoped_health["channels"].as_array().expect("array");
    assert_eq!(
        scoped_channels.len(),
        1,
        "a call scoped to tenant-a must see only tenant-a's row, not local's: {scoped_channels:?}"
    );
    assert_eq!(
        scoped_channels[0]["channel_slug"].as_str(),
        Some("tenant-a-inbox@example.com")
    );
    assert_eq!(
        scoped_health["namespace"].as_str(),
        Some("tenant-a"),
        "response must echo the explicitly-scoped namespace, not local: {scoped_health}"
    );
}

// ── #493: comm.inbox from_actor / from_prefix sender filter ─────────────────

/// A single actor namespace receives messages from two distinct senders;
/// `from_actor` (exact match) selects only the messages from the named sender.
#[tokio::test]
async fn t493_inbox_from_actor_filters_to_exact_sender() {
    let backend = shared_backend();
    let (registry_a, _rt_a) = build_actor_registry(backend.clone(), "lambda:a");
    let (registry_b, _rt_b) = build_actor_registry(backend.clone(), "lambda:b");
    let (registry_c, _rt_c) = build_actor_registry(backend.clone(), "lambda:c");

    registry_a
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:c", "content": "hi from A" }),
        )
        .await
        .expect("A send succeeds");
    registry_b
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:c", "content": "hi from B" }),
        )
        .await
        .expect("B send succeeds");

    let filtered = registry_c
        .dispatch(
            "comm.inbox",
            serde_json::json!({ "status": "all", "limit": 50, "from_actor": "lambda:a" }),
        )
        .await
        .expect("filtered inbox succeeds");
    let messages = filtered["messages"].as_array().expect("messages array");
    assert_eq!(
        messages.len(),
        1,
        "from_actor=lambda:a must return exactly 1 message; got {messages:?}"
    );
    assert_eq!(
        messages[0]["properties"]["from_actor"].as_str(),
        Some("lambda:a")
    );
}

/// `from_prefix` selects all senders whose actor label starts with the given prefix,
/// e.g. `"agent:khive:"` selects every spawned agent under that namespace.
#[tokio::test]
async fn t493_inbox_from_prefix_filters_to_matching_senders() {
    let backend = shared_backend();
    let (registry_a1, _rt_a1) = build_actor_registry(backend.clone(), "agent:khive:role-1");
    let (registry_a2, _rt_a2) = build_actor_registry(backend.clone(), "agent:khive:role-2");
    let (registry_other, _rt_other) = build_actor_registry(backend.clone(), "lambda:other");
    let (registry_c, _rt_c) = build_actor_registry(backend.clone(), "lambda:c");

    registry_a1
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:c", "content": "status from role-1" }),
        )
        .await
        .expect("a1 send succeeds");
    registry_a2
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:c", "content": "status from role-2" }),
        )
        .await
        .expect("a2 send succeeds");
    registry_other
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:c", "content": "unrelated message" }),
        )
        .await
        .expect("other send succeeds");

    let filtered = registry_c
        .dispatch(
            "comm.inbox",
            serde_json::json!({ "status": "all", "limit": 50, "from_prefix": "agent:khive:" }),
        )
        .await
        .expect("filtered inbox succeeds");
    let messages = filtered["messages"].as_array().expect("messages array");
    assert_eq!(
        messages.len(),
        2,
        "from_prefix=agent:khive: must return the 2 agent messages, excluding lambda:other; got {messages:?}"
    );
    for m in messages {
        let from_actor = m["properties"]["from_actor"].as_str().unwrap_or("");
        assert!(
            from_actor.starts_with("agent:khive:"),
            "every returned message must have a from_actor matching the prefix; got {from_actor:?}"
        );
    }
}

/// Supplying both `from_actor` and `from_prefix` is a per-op error naming the conflict.
#[tokio::test]
async fn t493_inbox_from_actor_and_from_prefix_mutually_exclusive() {
    let backend = shared_backend();
    let (registry, _rt) = build_actor_registry(backend, "lambda:c");

    let result = registry
        .dispatch(
            "comm.inbox",
            serde_json::json!({
                "from_actor": "lambda:a",
                "from_prefix": "agent:khive:",
            }),
        )
        .await;
    assert!(
        result.is_err(),
        "from_actor + from_prefix together must be rejected; got {result:?}"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("mutually exclusive"),
        "error must name the conflict; got: {err}"
    );
}

/// Absent from_actor/from_prefix preserves today's behavior exactly: no sender filter
/// is applied and both senders' messages are returned.
#[tokio::test]
async fn t493_inbox_without_sender_filter_returns_all_senders() {
    let backend = shared_backend();
    let (registry_a, _rt_a) = build_actor_registry(backend.clone(), "lambda:a");
    let (registry_b, _rt_b) = build_actor_registry(backend.clone(), "lambda:b");
    let (registry_c, _rt_c) = build_actor_registry(backend.clone(), "lambda:c");

    registry_a
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:c", "content": "hi from A" }),
        )
        .await
        .expect("A send succeeds");
    registry_b
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:c", "content": "hi from B" }),
        )
        .await
        .expect("B send succeeds");

    let unfiltered = registry_c
        .dispatch(
            "comm.inbox",
            serde_json::json!({ "status": "all", "limit": 50 }),
        )
        .await
        .expect("unfiltered inbox succeeds");
    let messages = unfiltered["messages"].as_array().expect("messages array");
    assert_eq!(
        messages.len(),
        2,
        "no sender filter must return both senders' messages unchanged; got {messages:?}"
    );
}

// ── #494: comm.thread tail pagination (order + after cursor) ────────────────
//
// NOTE: `comm.send`/`comm.reply` targeting the caller's own namespace ("local")
// write BOTH an outbound and an inbound copy of every logical message into that
// same namespace (dual_write_message, ADR-057) — so each `content` string below
// appears TWICE in an unfiltered thread(), consecutively (outbound then inbound),
// since the inbound copy is always written a moment after the outbound copy in
// the same call. Tests account for this pairing explicitly rather than assuming
// one physical note per logical send (matches the existing #485/H3 tests' use of
// tolerant `>=` counts for the same reason).

/// Default order ("asc") truncates from the tail — this is the pre-existing
/// (buggy, per #494) behavior that must stay byte-identical: a thread longer
/// than `limit` returns the HEAD (oldest messages), not the newest.
#[tokio::test]
async fn t494_thread_default_order_truncates_head_unchanged() {
    let (registry, _rt) = build_registry_for_ns("local");

    let root = registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "local", "content": "root" }),
        )
        .await
        .expect("root send succeeds");
    let root_full_id = root["full_id"].as_str().expect("root full_id").to_string();

    for i in 1..=4 {
        registry
            .dispatch(
                "comm.reply",
                serde_json::json!({ "id": root_full_id, "content": format!("reply-{i}") }),
            )
            .await
            .unwrap_or_else(|e| panic!("reply-{i} succeeds: {e:?}"));
    }

    // 5 logical messages (root + 4 replies) = 10 physical notes (outbound+inbound
    // pairs). limit=2 with no order= must return the OLDEST 2 physical notes —
    // both copies of "root" — matching pre-#494 truncate-from-head behavior.
    let result = registry
        .dispatch(
            "comm.thread",
            serde_json::json!({ "id": root_full_id, "limit": 2 }),
        )
        .await
        .expect("thread succeeds");
    let msgs = result["messages"].as_array().expect("messages array");
    assert_eq!(msgs.len(), 2, "limit=2 must return exactly 2 messages");
    let contents: Vec<&str> = msgs
        .iter()
        .map(|m| m["content"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(
        contents,
        vec!["root", "root"],
        "default order must truncate from the tail (keep the head), unchanged from before #494"
    );
}

/// `order="desc"` returns the newest `limit` messages instead of the oldest — the
/// #494 fix: long threads can now reach their tail.
#[tokio::test]
async fn t494_thread_order_desc_returns_newest_messages() {
    let (registry, _rt) = build_registry_for_ns("local");

    let root = registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "local", "content": "root" }),
        )
        .await
        .expect("root send succeeds");
    let root_full_id = root["full_id"].as_str().expect("root full_id").to_string();

    for i in 1..=4 {
        registry
            .dispatch(
                "comm.reply",
                serde_json::json!({ "id": root_full_id, "content": format!("reply-{i}") }),
            )
            .await
            .unwrap_or_else(|e| panic!("reply-{i} succeeds: {e:?}"));
    }

    let result = registry
        .dispatch(
            "comm.thread",
            serde_json::json!({ "id": root_full_id, "limit": 2, "order": "desc" }),
        )
        .await
        .expect("thread succeeds");
    let msgs = result["messages"].as_array().expect("messages array");
    assert_eq!(msgs.len(), 2, "limit=2 must return exactly 2 messages");
    let contents: Vec<&str> = msgs
        .iter()
        .map(|m| m["content"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(
        contents,
        vec!["reply-4", "reply-4"],
        "order=desc + limit=2 must return the newest 2 physical notes — both copies \
         of the last reply — not the oldest (#494 fix: the tail is now reachable)"
    );
}

/// An invalid `order` value is rejected, naming the valid set (ADR-084 Rule 2).
#[tokio::test]
async fn t494_thread_invalid_order_rejected() {
    let (registry, _rt) = build_registry_for_ns("local");

    let root = registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "local", "content": "root" }),
        )
        .await
        .expect("root send succeeds");
    let root_full_id = root["full_id"].as_str().expect("root full_id").to_string();

    let result = registry
        .dispatch(
            "comm.thread",
            serde_json::json!({ "id": root_full_id, "order": "banana" }),
        )
        .await;
    assert!(
        result.is_err(),
        "order=banana must be rejected; got {result:?}"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("asc") && err.contains("desc"),
        "error must name the valid order values; got: {err}"
    );
}

/// `after` accepts a message id cursor and returns only messages strictly after it
/// (enables incremental polling without re-fetching history). The cursor resolves
/// to the OUTBOUND copy's `full_id` (what `comm.reply` returns); its own inbound
/// copy — created a moment later in the same dual-write call — is strictly after
/// it and so is included.
#[tokio::test]
async fn t494_thread_after_id_cursor_returns_strictly_later_messages() {
    let (registry, _rt) = build_registry_for_ns("local");

    let root = registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "local", "content": "root" }),
        )
        .await
        .expect("root send succeeds");
    let root_full_id = root["full_id"].as_str().expect("root full_id").to_string();

    let reply1 = registry
        .dispatch(
            "comm.reply",
            serde_json::json!({ "id": root_full_id, "content": "reply-1" }),
        )
        .await
        .expect("reply-1 succeeds");
    let reply1_full_id = reply1["full_id"]
        .as_str()
        .expect("reply1 full_id")
        .to_string();

    let result = registry
        .dispatch(
            "comm.thread",
            serde_json::json!({ "id": root_full_id, "after": reply1_full_id }),
        )
        .await
        .expect("thread succeeds");
    let msgs = result["messages"].as_array().expect("messages array");
    let contents: Vec<&str> = msgs
        .iter()
        .map(|m| m["content"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(
        contents,
        vec!["reply-1"],
        "after=reply-1's outbound id must return only its own inbound copy \
         (strictly later), excluding root and reply-1's own outbound copy; got {contents:?}"
    );
}

/// Insert a `message` note directly into the store with an explicit `created_at`,
/// bypassing `comm.send`/`comm.reply`. Cursor/tie-break/ordering tests need exact
/// control over timestamps (including two rows sharing the same microsecond) that
/// racing the wall clock through the normal dispatch path cannot guarantee.
async fn insert_thread_message(
    rt: &KhiveRuntime,
    ns: &str,
    id: uuid::Uuid,
    thread_id: uuid::Uuid,
    created_at: i64,
    content: &str,
) {
    let token = rt
        .authorize(Namespace::parse(ns).expect("valid namespace"))
        .expect("authorize");
    let store = rt.notes(&token).expect("notes store");
    store
        .upsert_note(khive_storage::note::Note {
            id,
            namespace: ns.to_string(),
            kind: "message".into(),
            status: "active".into(),
            name: None,
            content: content.to_string(),
            salience: None,
            decay_factor: None,
            expires_at: None,
            properties: Some(serde_json::json!({
                "direction": "inbound",
                "from": "x",
                "to": ns,
                "read": false,
                "thread_id": thread_id.as_hyphenated().to_string(),
            })),
            created_at,
            updated_at: created_at,
            deleted_at: None,
        })
        .await
        .expect("insert message");
}

/// #494: two physical messages that share the exact same
/// microsecond `created_at` (e.g. what an ADR-057 dual-write self-send can
/// produce) must not be skipped or duplicated around an id cursor — the cursor
/// filter and sort must compare the full `(created_at, full_id)` tuple, not
/// timestamp alone.
#[tokio::test]
async fn t494_thread_after_id_cursor_ties_on_equal_created_at_no_skip_no_dup() {
    let (registry, rt) = build_registry_for_ns("local");

    let root = registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "local", "content": "root" }),
        )
        .await
        .expect("root send succeeds");
    let root_full_id = root["full_id"].as_str().expect("root full_id").to_string();
    let root_uuid = uuid::Uuid::parse_str(&root_full_id).unwrap();

    let tied_at = chrono::Utc::now().timestamp_micros();
    let uuid_lo = uuid::Uuid::parse_str("00000000-0000-4000-8000-000000000001").unwrap();
    let uuid_hi = uuid::Uuid::parse_str("ffffffff-0000-4000-8000-000000000002").unwrap();
    insert_thread_message(&rt, "local", uuid_lo, root_uuid, tied_at, "tied-lo").await;
    insert_thread_message(&rt, "local", uuid_hi, root_uuid, tied_at, "tied-hi").await;

    let after_lo = registry
        .dispatch(
            "comm.thread",
            serde_json::json!({ "id": root_full_id, "after": uuid_lo.to_string() }),
        )
        .await
        .expect("thread succeeds");
    let contents_lo: Vec<&str> = after_lo["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["content"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(
        contents_lo,
        vec!["tied-hi"],
        "after=lo must return exactly the higher-uuid tied row once, not skip or \
         duplicate it; got {contents_lo:?}"
    );

    let after_hi = registry
        .dispatch(
            "comm.thread",
            serde_json::json!({ "id": root_full_id, "after": uuid_hi.to_string() }),
        )
        .await
        .expect("thread succeeds");
    let contents_hi: Vec<&str> = after_hi["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["content"].as_str().unwrap_or(""))
        .collect();
    assert!(
        contents_hi.is_empty(),
        "after=hi must return nothing — hi is the greatest key among the tied rows; \
         got {contents_hi:?}"
    );
}

/// #494: an `after` timestamp cursor must be parsed to
/// microseconds (not compared as a raw string) so non-canonical but valid RFC
/// 3339 forms — whole-second `Z`, or an explicit `+00:00` offset — compare
/// correctly against khive's canonical microsecond timestamps.
#[tokio::test]
async fn t494_thread_after_timestamp_cursor_accepts_noncanonical_rfc3339() {
    let (registry, rt) = build_registry_for_ns("local");

    let root = registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "local", "content": "root" }),
        )
        .await
        .expect("root send succeeds");
    let root_full_id = root["full_id"].as_str().expect("root full_id").to_string();
    let root_uuid = uuid::Uuid::parse_str(&root_full_id).unwrap();

    // Far in the future so the real-clock root note (created "now") is always
    // strictly before the cursor and never leaks into the filtered result.
    let ts1 = chrono::DateTime::parse_from_rfc3339("2099-01-01T00:00:00Z")
        .unwrap()
        .timestamp_micros();
    let ts2 = ts1 + 1_000_000;
    let id1 = uuid::Uuid::parse_str("11111111-0000-4000-8000-000000000001").unwrap();
    let id2 = uuid::Uuid::parse_str("22222222-0000-4000-8000-000000000002").unwrap();
    insert_thread_message(&rt, "local", id1, root_uuid, ts1, "at-ts1").await;
    insert_thread_message(&rt, "local", id2, root_uuid, ts2, "at-ts2").await;

    for cursor in ["2099-01-01T00:00:00Z", "2099-01-01T00:00:00+00:00"] {
        let result = registry
            .dispatch(
                "comm.thread",
                serde_json::json!({ "id": root_full_id, "after": cursor }),
            )
            .await
            .unwrap_or_else(|e| panic!("cursor {cursor:?} must parse: {e:?}"));
        let contents: Vec<&str> = result["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["content"].as_str().unwrap_or(""))
            .collect();
        assert_eq!(
            contents,
            vec!["at-ts2"],
            "whole-second/offset RFC3339 cursor {cursor:?} must exclude the note at \
             exactly that instant and include only the strictly-later one; got {contents:?}"
        );
    }
}

/// #494: an `after` value that is neither a resolvable
/// message id nor a parseable RFC 3339 timestamp must fail loudly, never be
/// silently coerced into "no cursor" (which would return the whole thread).
#[tokio::test]
async fn t494_thread_after_invalid_string_is_hard_error() {
    let (registry, _rt) = build_registry_for_ns("local");

    let root = registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "local", "content": "root" }),
        )
        .await
        .expect("root send succeeds");
    let root_full_id = root["full_id"].as_str().expect("root full_id").to_string();

    let result = registry
        .dispatch(
            "comm.thread",
            serde_json::json!({ "id": root_full_id, "after": "not-a-valid-cursor" }),
        )
        .await;
    assert!(
        result.is_err(),
        "an `after` value that is neither a resolvable id nor a valid RFC 3339 \
         timestamp must be a hard error, not silently treated as no-cursor; got {result:?}"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("neither a resolvable message id nor a valid RFC 3339 timestamp"),
        "error must name why the cursor was rejected; got: {err}"
    );
}

/// #494: `order="desc"` combined with an id `after` cursor
/// must filter against the DESC sequence, not always `created_at >`. "After" in
/// desc order means further along the desc traversal, i.e. strictly older.
#[tokio::test]
async fn t494_thread_order_desc_with_after_id_cursor_returns_strictly_older_in_desc_sequence() {
    let (registry, rt) = build_registry_for_ns("local");

    let root = registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "local", "content": "root" }),
        )
        .await
        .expect("root send succeeds");
    let root_full_id = root["full_id"].as_str().expect("root full_id").to_string();
    let root_uuid = uuid::Uuid::parse_str(&root_full_id).unwrap();

    let base = chrono::DateTime::parse_from_rfc3339("2099-01-01T00:00:00Z")
        .unwrap()
        .timestamp_micros();
    let id_a = uuid::Uuid::parse_str("aaaaaaaa-0000-4000-8000-000000000001").unwrap();
    let id_b = uuid::Uuid::parse_str("bbbbbbbb-0000-4000-8000-000000000002").unwrap();
    let id_c = uuid::Uuid::parse_str("cccccccc-0000-4000-8000-000000000003").unwrap();
    insert_thread_message(&rt, "local", id_a, root_uuid, base, "msg-a").await;
    insert_thread_message(&rt, "local", id_b, root_uuid, base + 1_000_000, "msg-b").await;
    insert_thread_message(&rt, "local", id_c, root_uuid, base + 2_000_000, "msg-c").await;

    // `comm.send(to="local", ...)` is a self-send: ADR-057 dual-write stores
    // both an outbound and an inbound copy of "root", both real-clock (and so
    // both strictly older than the synthetic 2099 timestamps). Full desc
    // sequence is therefore [msg-c, msg-b, msg-a, root, root]. `after=msg-b`
    // must return only what comes strictly after it in THAT sequence —
    // msg-a and both root copies — never msg-c, even though msg-c is also
    // `>` msg-b in wall-clock terms.
    let result = registry
        .dispatch(
            "comm.thread",
            serde_json::json!({ "id": root_full_id, "order": "desc", "after": id_b.to_string() }),
        )
        .await
        .expect("thread succeeds");
    let contents: Vec<&str> = result["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["content"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(
        contents,
        vec!["msg-a", "root", "root"],
        "order=desc + after=msg-b must return only rows strictly older than msg-b \
         (further along the desc sequence), in desc order — both self-send root \
         copies included, msg-c excluded; got {contents:?}"
    );
}

/// Absent `order`/`after` preserves today's behavior exactly: same messages, same order,
/// same truncation as before #494 (regression guard alongside the existing #485 test).
#[tokio::test]
async fn t494_thread_without_new_params_unchanged() {
    let (registry, _rt) = build_registry_for_ns("local");

    let root = registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "local", "content": "root" }),
        )
        .await
        .expect("root send succeeds");
    let root_full_id = root["full_id"].as_str().expect("root full_id").to_string();

    registry
        .dispatch(
            "comm.reply",
            serde_json::json!({ "id": root_full_id, "content": "reply-1" }),
        )
        .await
        .expect("reply-1 succeeds");

    let result = registry
        .dispatch("comm.thread", serde_json::json!({ "id": root_full_id }))
        .await
        .expect("thread succeeds");
    let msgs = result["messages"].as_array().expect("messages array");
    assert_eq!(
        msgs.len(),
        4,
        "root (outbound+inbound) + reply-1 (outbound+inbound) = 4 physical notes"
    );
    let contents: Vec<&str> = msgs
        .iter()
        .map(|m| m["content"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(contents, vec!["root", "root", "reply-1", "reply-1"]);
}

// ── #495: comm.send / comm.reply metadata (tags) passthrough ────────────────

/// `comm.send(tags=[...])` persists the tags into `properties["tags"]` on the
/// inbound copy, round-tripped via `comm.inbox`.
#[tokio::test]
async fn t495_send_tags_roundtrip_via_inbox() {
    let backend = shared_backend();
    let (registry_a, _rt_a) = build_actor_registry(backend.clone(), "lambda:a");
    let (registry_b, _rt_b) = build_actor_registry(backend, "lambda:b");

    registry_a
        .dispatch(
            "comm.send",
            serde_json::json!({
                "to": "lambda:b",
                "content": "tagged message",
                "tags": ["run:abc123", "traffic:agent"],
            }),
        )
        .await
        .expect("tagged send succeeds");

    let inbox = registry_b
        .dispatch(
            "comm.inbox",
            serde_json::json!({ "status": "all", "limit": 10 }),
        )
        .await
        .expect("inbox succeeds");
    let messages = inbox["messages"].as_array().expect("messages array");
    assert_eq!(messages.len(), 1);
    let tags = messages[0]["properties"]["tags"]
        .as_array()
        .expect("tags array present on inbound copy");
    let tag_strs: Vec<&str> = tags.iter().map(|t| t.as_str().unwrap_or("")).collect();
    assert_eq!(tag_strs, vec!["run:abc123", "traffic:agent"]);
}

/// `comm.send(tags=[...])` also persists on the outbound copy, round-tripped
/// via `comm.read` after resolving the sender's own outbound note.
#[tokio::test]
async fn t495_send_tags_present_on_outbound_copy() {
    let backend = shared_backend();
    let (registry_a, rt_a) = build_actor_registry(backend, "lambda:a");

    let send_result = registry_a
        .dispatch(
            "comm.send",
            serde_json::json!({
                "to": "lambda:a",
                "content": "self-tagged",
                "tags": ["job:42"],
                "self_send": true,
            }),
        )
        .await
        .expect("tagged self-send succeeds");
    let outbound_full_id = send_result["full_id"].as_str().expect("full_id");
    let outbound_uuid: uuid::Uuid = outbound_full_id.parse().expect("valid uuid");

    let tok = rt_a.authorize(Namespace::parse("local").unwrap()).unwrap();
    let store = rt_a.notes(&tok).expect("notes store");
    let note = store
        .get_note(outbound_uuid)
        .await
        .expect("get_note succeeds")
        .expect("outbound note exists");
    let tags = note.properties.as_ref().and_then(|p| p.get("tags"));
    assert_eq!(
        tags,
        Some(&serde_json::json!(["job:42"])),
        "outbound copy must also carry tags"
    );
}

/// `comm.reply(tags=[...])` persists tags on the reply's inbound copy.
#[tokio::test]
async fn t495_reply_tags_roundtrip_via_inbox() {
    let backend = shared_backend();
    let (registry_a, _rt_a) = build_actor_registry(backend.clone(), "lambda:a");
    let (registry_b, _rt_b) = build_actor_registry(backend, "lambda:b");

    registry_a
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:b", "content": "hello" }),
        )
        .await
        .expect("send succeeds");

    let inbox_b = registry_b
        .dispatch(
            "comm.inbox",
            serde_json::json!({ "status": "all", "limit": 10 }),
        )
        .await
        .expect("B inbox succeeds");
    let b_inbound_id = inbox_b["messages"][0]["full_id"]
        .as_str()
        .expect("B inbound full_id");

    registry_b
        .dispatch(
            "comm.reply",
            serde_json::json!({
                "id": b_inbound_id,
                "content": "reply with tags",
                "tags": ["job:reply-1"],
            }),
        )
        .await
        .expect("tagged reply succeeds");

    let inbox_a = registry_a
        .dispatch(
            "comm.inbox",
            serde_json::json!({ "status": "all", "limit": 10 }),
        )
        .await
        .expect("A inbox succeeds");
    let a_messages = inbox_a["messages"].as_array().expect("messages array");
    let reply_msg = a_messages
        .iter()
        .find(|m| m["content"] == "reply with tags")
        .expect("A's inbox contains the tagged reply");
    let tags = reply_msg["properties"]["tags"]
        .as_array()
        .expect("tags array present on reply's inbound copy");
    let tag_strs: Vec<&str> = tags.iter().map(|t| t.as_str().unwrap_or("")).collect();
    assert_eq!(tag_strs, vec!["job:reply-1"]);
}

/// Absent `tags` preserves today's behavior exactly: no `properties["tags"]` key at all.
#[tokio::test]
async fn t495_send_without_tags_omits_tags_property() {
    let backend = shared_backend();
    let (registry_a, _rt_a) = build_actor_registry(backend.clone(), "lambda:a");
    let (registry_b, _rt_b) = build_actor_registry(backend, "lambda:b");

    registry_a
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:b", "content": "no tags here" }),
        )
        .await
        .expect("send succeeds");

    let inbox = registry_b
        .dispatch(
            "comm.inbox",
            serde_json::json!({ "status": "all", "limit": 10 }),
        )
        .await
        .expect("inbox succeeds");
    let messages = inbox["messages"].as_array().expect("messages array");
    assert_eq!(messages.len(), 1);
    assert!(
        messages[0]["properties"].get("tags").is_none(),
        "absent tags must not add a properties.tags key; got {:?}",
        messages[0]["properties"]
    );
}

/// `comm.send` with an unknown top-level field (typo) is still rejected —
/// `tags` addition must not have loosened `deny_unknown_fields`.
#[tokio::test]
async fn t495_send_rejects_unknown_field_alongside_tags() {
    let (registry, _rt) = build_registry_for_ns("local");

    let result = registry
        .dispatch(
            "comm.send",
            serde_json::json!({
                "to": "local",
                "content": "hi",
                "tags": ["a"],
                "bogus_field": "typo",
            }),
        )
        .await;
    assert!(
        result.is_err(),
        "unknown field alongside tags must still be rejected; got {result:?}"
    );
}

// --- channel poll checkpoint persistence tests (khive #449) ---

#[tokio::test]
async fn cursor_get_returns_none_for_new_mailbox() {
    let (registry, _rt) = build_registry();

    let result = registry
        .dispatch(
            "comm.cursor_get",
            serde_json::json!({ "channel_kind": "email", "channel_slug": "acct-1" }),
        )
        .await
        .expect("cursor_get succeeds for a mailbox with no prior checkpoint");
    assert!(
        result.is_null(),
        "an unseeded (channel_kind, channel_slug) must read back null; got {result}"
    );
}

#[tokio::test]
async fn cursor_commit_round_trips_generation_high_water_and_time() {
    let (registry, _rt) = build_registry();

    let committed = registry
        .dispatch(
            "comm.cursor_commit",
            serde_json::json!({
                "channel_kind": "email",
                "channel_slug": "acct-1",
                "source": "imap+tls:mail.example.com:993:inbox@example.com:INBOX",
                "generation": 17,
                "high_water": 42,
            }),
        )
        .await
        .expect("cursor_commit succeeds");
    assert_eq!(committed["generation"], 17);
    assert_eq!(committed["high_water"], 42);
    assert!(
        committed["committed_at"].as_str().is_some(),
        "cursor_commit must return an RFC3339 committed_at; got {committed}"
    );

    let fetched = registry
        .dispatch(
            "comm.cursor_get",
            serde_json::json!({ "channel_kind": "email", "channel_slug": "acct-1" }),
        )
        .await
        .expect("cursor_get succeeds");
    assert_eq!(
        fetched["source"],
        "imap+tls:mail.example.com:993:inbox@example.com:INBOX"
    );
    assert_eq!(fetched["generation"], 17);
    assert_eq!(fetched["high_water"], 42);
    assert!(
        fetched["committed_at"].as_str().is_some(),
        "cursor_get must round-trip an RFC3339 committed_at; got {fetched}"
    );
}

#[tokio::test]
async fn cursor_rows_are_isolated_by_kind_and_slug() {
    let (registry, _rt) = build_registry();

    registry
        .dispatch(
            "comm.cursor_commit",
            serde_json::json!({
                "channel_kind": "email",
                "channel_slug": "acct-1",
                "source": "imap+tls:host-a:993:a@example.com:INBOX",
                "generation": 1,
                "high_water": 5,
            }),
        )
        .await
        .expect("commit for acct-1 succeeds");
    registry
        .dispatch(
            "comm.cursor_commit",
            serde_json::json!({
                "channel_kind": "email",
                "channel_slug": "acct-2",
                "source": "imap+tls:host-b:993:b@example.com:INBOX",
                "generation": 9,
                "high_water": 99,
            }),
        )
        .await
        .expect("commit for acct-2 succeeds");

    let acct_1 = registry
        .dispatch(
            "comm.cursor_get",
            serde_json::json!({ "channel_kind": "email", "channel_slug": "acct-1" }),
        )
        .await
        .expect("cursor_get acct-1 succeeds");
    let acct_2 = registry
        .dispatch(
            "comm.cursor_get",
            serde_json::json!({ "channel_kind": "email", "channel_slug": "acct-2" }),
        )
        .await
        .expect("cursor_get acct-2 succeeds");

    assert_eq!(
        acct_1["high_water"], 5,
        "acct-1's row must not see acct-2's write"
    );
    assert_eq!(
        acct_2["high_water"], 99,
        "acct-2's row must not see acct-1's write"
    );
}

#[tokio::test]
async fn cursor_uidvalidity_reset_can_replace_high_water_with_null() {
    let (registry, _rt) = build_registry();

    registry
        .dispatch(
            "comm.cursor_commit",
            serde_json::json!({
                "channel_kind": "email",
                "channel_slug": "acct-1",
                "source": "imap+tls:host:993:a@example.com:INBOX",
                "generation": 1,
                "high_water": 50,
            }),
        )
        .await
        .expect("initial commit succeeds");

    // A UIDVALIDITY change resets the epoch; the new generation carries no
    // high_water yet because nothing has been fetched in the new epoch.
    registry
        .dispatch(
            "comm.cursor_commit",
            serde_json::json!({
                "channel_kind": "email",
                "channel_slug": "acct-1",
                "source": "imap+tls:host:993:a@example.com:INBOX",
                "generation": 2,
            }),
        )
        .await
        .expect("reset commit succeeds");

    let fetched = registry
        .dispatch(
            "comm.cursor_get",
            serde_json::json!({ "channel_kind": "email", "channel_slug": "acct-1" }),
        )
        .await
        .expect("cursor_get succeeds");
    assert_eq!(fetched["generation"], 2);
    assert!(
        fetched["high_water"].is_null(),
        "an UIDVALIDITY-reset commit must be able to replace a prior high_water with null; got {fetched}"
    );
}

#[tokio::test]
async fn cursor_commit_rejects_empty_identity_zero_or_i64_overflow() {
    let (registry, _rt) = build_registry();

    let empty_kind = registry
        .dispatch(
            "comm.cursor_commit",
            serde_json::json!({
                "channel_kind": "",
                "channel_slug": "acct-1",
                "source": "imap+tls:host:993:a@example.com:INBOX",
                "generation": 1,
            }),
        )
        .await;
    assert!(empty_kind.is_err(), "empty channel_kind must be rejected");

    let empty_source = registry
        .dispatch(
            "comm.cursor_commit",
            serde_json::json!({
                "channel_kind": "email",
                "channel_slug": "acct-1",
                "source": "",
                "generation": 1,
            }),
        )
        .await;
    assert!(empty_source.is_err(), "empty source must be rejected");

    let zero_generation = registry
        .dispatch(
            "comm.cursor_commit",
            serde_json::json!({
                "channel_kind": "email",
                "channel_slug": "acct-1",
                "source": "imap+tls:host:993:a@example.com:INBOX",
                "generation": 0,
            }),
        )
        .await;
    assert!(zero_generation.is_err(), "generation=0 must be rejected");

    let zero_high_water = registry
        .dispatch(
            "comm.cursor_commit",
            serde_json::json!({
                "channel_kind": "email",
                "channel_slug": "acct-1",
                "source": "imap+tls:host:993:a@example.com:INBOX",
                "generation": 1,
                "high_water": 0,
            }),
        )
        .await;
    assert!(zero_high_water.is_err(), "high_water=0 must be rejected");

    let overflowing_generation = registry
        .dispatch(
            "comm.cursor_commit",
            serde_json::json!({
                "channel_kind": "email",
                "channel_slug": "acct-1",
                "source": "imap+tls:host:993:a@example.com:INBOX",
                "generation": u64::MAX,
            }),
        )
        .await;
    assert!(
        overflowing_generation.is_err(),
        "generation beyond i64::MAX must be rejected, not silently truncated"
    );
}

#[tokio::test]
async fn cursor_schema_lazy_bootstraps_fresh_memory_runtime() {
    // A fresh in-memory runtime never runs the boot-time schema plan the way
    // a real daemon startup does; cursor_get/cursor_commit must still work by
    // lazily applying the idempotent CREATE TABLE IF NOT EXISTS statement
    // themselves, exactly like the rest of the pack's lazy-bootstrap tests.
    let (registry, _rt) = build_registry();

    let before = registry
        .dispatch(
            "comm.cursor_get",
            serde_json::json!({ "channel_kind": "email", "channel_slug": "fresh" }),
        )
        .await
        .expect("cursor_get on a never-written table must not error, just return null");
    assert!(before.is_null());

    let committed = registry
        .dispatch(
            "comm.cursor_commit",
            serde_json::json!({
                "channel_kind": "email",
                "channel_slug": "fresh",
                "source": "imap+tls:host:993:a@example.com:INBOX",
                "generation": 1,
                "high_water": 1,
            }),
        )
        .await
        .expect("cursor_commit on a never-written table must lazily create the schema and succeed");
    assert_eq!(committed["generation"], 1);
}

// ── Issue #820: sub-agent self-address must be loud, not silent ─────────────
//
// A sub-agent session spawned in the same project scope resolves its actor
// identity from the same worktree-scoped `.khive/config.toml` as its parent
// orchestrator (ADR-096 Fork 2: `[actor] id` is a per-project, not per-session,
// injection tier). When the sub-agent addresses that shared label thinking it
// reaches a distinct parent principal, `from_actor` and `to_actor` collapse
// onto the identical string with no error and no distinct inbox.

/// Child and parent configured with genuinely distinct actor identities: a
/// send from the child to the parent's label must succeed and land in the
/// parent's inbox only, exactly as ordinary actor-addressed delivery already
/// works (ADR-057). This is the "no bug" baseline the fix must not regress.
#[tokio::test]
async fn i820_child_to_parent_delivery_with_distinct_identities_succeeds() {
    let backend = shared_backend();
    let (registry_child, _rt_child) = build_actor_registry(backend.clone(), "lambda:child");
    let (registry_parent, _rt_parent) = build_actor_registry(backend, "lambda:parent");

    let sent = registry_child
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:parent", "content": "status update from child" }),
        )
        .await
        .expect("child->parent send with distinct actor identities must succeed");
    assert_eq!(sent["to"], serde_json::json!("lambda:parent"));
    assert_eq!(sent["from"], serde_json::json!("lambda:child"));

    let parent_inbox = registry_parent
        .dispatch(
            "comm.inbox",
            serde_json::json!({ "status": "all", "limit": 50 }),
        )
        .await
        .expect("parent inbox succeeds");
    assert_eq!(
        parent_inbox["count"], 1,
        "parent must see exactly 1 message addressed to lambda:parent; got {parent_inbox}"
    );

    let child_inbox = registry_child
        .dispatch(
            "comm.inbox",
            serde_json::json!({ "status": "all", "limit": 50 }),
        )
        .await
        .expect("child inbox succeeds");
    assert_eq!(
        child_inbox["count"], 0,
        "child must not see the message it addressed to a distinct parent; got {child_inbox}"
    );
}

/// A caller whose named target genuinely IS its own resolved actor identity
/// (a deliberate note-to-self) must still be allowed to send when it says so
/// explicitly via `self_send=true`.
#[tokio::test]
async fn i820_explicit_self_send_allowed_when_flagged() {
    let (registry, _rt) = build_actor_registry(shared_backend(), "lambda:leo");

    let sent = registry
        .dispatch(
            "comm.send",
            serde_json::json!({
                "to": "lambda:leo",
                "content": "reminder to self",
                "self_send": true,
            }),
        )
        .await
        .expect("explicit self-send must be allowed when self_send=true");
    assert_eq!(sent["to"], serde_json::json!("lambda:leo"));
    assert_eq!(sent["from"], serde_json::json!("lambda:leo"));

    let inbox = registry
        .dispatch(
            "comm.inbox",
            serde_json::json!({ "status": "all", "limit": 50 }),
        )
        .await
        .expect("inbox succeeds");
    assert_eq!(
        inbox["count"], 1,
        "self-sent note must be visible in the inbox"
    );
}

/// The silent-collapse case: a session addresses a label that happens to equal
/// its own resolved actor identity (e.g. a sub-agent naming what it believes is
/// its parent's distinct label, but which resolves to the same `[actor] id` as
/// its own token per ADR-096 Fork 2) WITHOUT declaring `self_send=true`. This
/// must now be a loud error, never a silent delivery into the sender's own
/// inbox.
#[tokio::test]
async fn i820_unflagged_self_address_is_a_loud_error() {
    let (registry, _rt) = build_actor_registry(shared_backend(), "lambda:leo");

    let result = registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:leo", "content": "meant for my parent" }),
        )
        .await;
    assert!(
        result.is_err(),
        "an unflagged send whose resolved target equals the sender's own actor identity \
         must error, not silently self-address; got {result:?}"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("self-address") || err.contains("self_send"),
        "error must explain the self-address collapse and the self_send escape hatch; got: {err}"
    );

    let inbox = registry
        .dispatch(
            "comm.inbox",
            serde_json::json!({ "status": "all", "limit": 50 }),
        )
        .await
        .expect("inbox succeeds");
    assert_eq!(
        inbox["count"], 0,
        "a rejected send must not leave a message behind in any inbox"
    );
}

/// The anonymous single-tenant party-line default (`to="local"` from an
/// unattributed caller) must remain unaffected: `to_actor == "local"` is
/// exempted from the self-address rejection since it is the pervasive
/// unconfigured single-actor pattern, not a collapsed distinct-principal
/// address.
#[tokio::test]
async fn i820_anonymous_local_party_line_send_still_succeeds() {
    let (registry, _rt) = build_registry();

    let result = registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "local", "content": "party line message" }),
        )
        .await;
    assert!(
        result.is_ok(),
        "unattributed to=local send must not be rejected by the #820 self-address guard; \
         got {result:?}"
    );
}
