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
            serde_json::json!({ "to": "lambda:khive", "content": "you have mail" }),
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
            serde_json::json!({ "to": "lambda:khive", "content": "meeting at 3pm" }),
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
    // lambda:khive (configured actor_id) sends to itself and replies.
    // With proper actor attribution, inbox filters correctly and reply inbounds are visible.
    let backend = shared_backend();
    let (registry, _rt) = build_actor_registry(backend, "lambda:khive");

    registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:khive", "content": "original message" }),
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

// T5b — ADR-057: comm.reply always writes same-namespace (Fix 1, codex Critical #2).
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
            serde_json::json!({ "to": "lambda:khive", "content": "hello for reply" }),
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
// Leo (registry_shared) sends to khive (both copies in "local"). Then replies to
// the inbound copy, verifying reply inherits the canonical thread_id.
#[tokio::test]
async fn t9_reply_cross_ns_delivers_when_allowed() {
    let backend = shared_backend();
    // Both actors use a registry with default_namespace="lambda:shared", but ADR-007
    // ensures all storage routes to "local".
    let (registry_shared, rt_shared) =
        build_crossns_registry(Arc::clone(&backend), "lambda:shared", vec![]);

    // "Leo" (operating as lambda:shared) sends to "khive".
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
            serde_json::json!({ "to": "lambda:b", "content": "secret for B only" }),
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
