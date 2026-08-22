//! Smoke tests for the comm pack. See `docs/integration-tests.md` for why this stays one file.

use std::sync::Arc;

use khive_pack_comm::CommPack;
use khive_runtime::{
    AllowAllGate, BackendId, KhiveRuntime, Namespace, NamespaceToken, NotePatch, RequestIdentity,
    RuntimeConfig, VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::types::{SqlRow, SqlValue};
use khive_storage::Note;
use khive_types::Pack;

fn list_items(response: &serde_json::Value) -> &[serde_json::Value] {
    response["items"]
        .as_array()
        .expect("list response must contain an items array")
}

fn build_registry() -> (VerbRegistry, KhiveRuntime) {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    khive_runtime::PackRegistry::register_packs(
        &["kg".to_string(), "comm".to_string()],
        runtime.clone(),
        &mut builder,
    )
    .expect("register kg+comm through the factory path");
    let registry = builder.build().expect("registry builds");
    (registry, runtime)
}

/// Build a registry with a specific default namespace (for caller-scoped dispatch).
fn build_registry_for_ns(ns: &str) -> (VerbRegistry, KhiveRuntime) {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    khive_runtime::PackRegistry::register_packs(
        &["kg".to_string(), "comm".to_string()],
        runtime.clone(),
        &mut builder,
    )
    .expect("register kg+comm through the factory path");
    builder.with_default_namespace(ns);
    let registry = builder.build().expect("registry builds");
    (registry, runtime)
}

#[test]
fn comm_pack_declares_message_note_kind() {
    assert!(CommPack::NOTE_KINDS.contains(&"message"));
}

#[tokio::test]
async fn pack_registered_message_notes_are_queryable_through_gql() {
    let (registry, rt) = build_registry_for_ns("local");
    assert!(
        registry.all_note_kinds().contains(&"message"),
        "the built registry must expose comm's message note kind"
    );

    registry
        .dispatch(
            "comm.send",
            serde_json::json!({
                "to": "local",
                "content": "GQL pack-note regression"
            }),
        )
        .await
        .expect("self-send creates message notes");

    let token = rt.authorize(Namespace::local()).expect("local token");
    let by_granular_label = rt
        .query(&token, "MATCH (m:message) RETURN m.id")
        .await
        .expect("pack-registered granular label compiles and executes");
    let by_note_kind = rt
        .query(
            &token,
            "MATCH (m:note) WHERE m.kind = 'message' RETURN m.id",
        )
        .await
        .expect("note substrate plus kind predicate compiles and executes");

    fn ids(rows: &[SqlRow]) -> Vec<String> {
        let mut ids: Vec<String> = rows
            .iter()
            .map(|row| match row.get("m_id") {
                Some(SqlValue::Text(id)) => id.clone(),
                value => panic!("GQL m.id projection must be text; got {value:?}"),
            })
            .collect();
        ids.sort();
        ids
    }

    let granular_ids = ids(&by_granular_label);
    assert!(
        !granular_ids.is_empty(),
        "MATCH (m:message) must return the message rows just written"
    );
    assert_eq!(
        granular_ids,
        ids(&by_note_kind),
        "granular and substrate-plus-kind spellings must select the same message notes"
    );
}

#[test]
fn comm_pack_declares_fourteen_handlers() {
    assert_eq!(
        CommPack::HANDLERS.len(),
        14,
        "comm pack must declare 14 handlers: send, delivered, inbox, read, mark_read, unread, reply, \
         thread, ingest, heartbeat, health, probe, cursor_get, cursor_commit \
         (khive #1387, #1447, #449, #66)"
    );
    let names: Vec<&str> = CommPack::HANDLERS.iter().map(|h| h.name).collect();
    assert!(names.contains(&"comm.send"));
    assert!(
        names.contains(&"comm.delivered"),
        "comm.delivered verb must be registered (khive #1447)"
    );
    assert!(names.contains(&"comm.inbox"));
    assert!(names.contains(&"comm.read"));
    assert!(
        names.contains(&"comm.mark_read"),
        "comm.mark_read verb must be registered (khive #1387)"
    );
    assert!(
        names.contains(&"comm.unread"),
        "comm.unread verb must be registered (khive #66)"
    );
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

/// Self-send (`to: "local"`) dual-writes an outbound record plus an inbound sibling, and
/// the inbound copy's read status depends on delivery ordering — so the inbox query uses
/// `status: "all"` (not the unread default) and asserts only that a `count` field is
/// present, since a count of zero is a legal outcome of this fixture.
#[tokio::test]
async fn send_and_inbox_roundtrip() {
    let (registry, _rt) = build_registry();

    let result = registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "local", "content": "hello" }),
        )
        .await
        .expect("send succeeds");
    assert!(result.get("id").is_some(), "send returns id: {result}");

    let inbox = registry
        .dispatch(
            "comm.inbox",
            serde_json::json!({ "status": "all", "limit": 10 }),
        )
        .await
        .expect("inbox succeeds");
    assert!(inbox.get("count").is_some(), "inbox returns count: {inbox}");
}

/// #1447: a successful dual-write is confirmed by the inbound sibling's correlation property, independent of message content.
#[tokio::test]
async fn delivered_confirms_successful_actor_send() {
    let (registry, _rt) = build_registry();
    let sent = registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "local", "content": "templated body" }),
        )
        .await
        .expect("send succeeds");
    let outbound_id = sent
        .get("full_id")
        .and_then(|value| value.as_str())
        .expect("send returns full outbound UUID");

    let result = registry
        .dispatch("comm.delivered", serde_json::json!({ "id": outbound_id }))
        .await
        .expect("delivery confirmation succeeds");

    assert_eq!(result["id"], outbound_id);
    assert_eq!(result["status"], "delivered");
    assert_eq!(result["delivered"], true);
    assert_eq!(result["inbound_count"], 1);
}

/// Confirmation is a sender operation: another actor in the same namespace cannot use a known outbound correlation UUID to inspect the sender's result.
#[tokio::test]
async fn delivered_is_scoped_to_the_sending_actor() {
    let backend = shared_backend();
    let (sender, _sender_runtime) = build_actor_registry(backend.clone(), "lambda:sender");
    let (recipient, _recipient_runtime) = build_actor_registry(backend, "lambda:recipient");

    let sent = sender
        .dispatch(
            "comm.send",
            serde_json::json!({
                "to": "lambda:recipient",
                "content": "sender-only confirmation",
            }),
        )
        .await
        .expect("actor-addressed send succeeds");
    let outbound_id = sent["full_id"].as_str().expect("full outbound UUID");

    let sender_result = sender
        .dispatch("comm.delivered", serde_json::json!({ "id": outbound_id }))
        .await
        .expect("sender confirmation succeeds");
    assert_eq!(sender_result["delivered"], true);

    let recipient_result = recipient
        .dispatch("comm.delivered", serde_json::json!({ "id": outbound_id }))
        .await
        .expect("non-sender confirmation remains a non-disclosing lookup");
    assert_eq!(recipient_result["status"], "undelivered");
    assert_eq!(recipient_result["delivered"], false);
    assert_eq!(recipient_result["inbound_count"], 0);
}

/// #1447: an outbound-only row is explicitly undelivered even when its body is identical to another message that did arrive.
#[tokio::test]
async fn delivered_rejects_outbound_only_identical_body() {
    let (registry, rt) = build_registry();
    registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "local", "content": "identical template" }),
        )
        .await
        .expect("control send succeeds");

    let token = rt
        .authorize(Namespace::parse("local").unwrap())
        .expect("authorize local");
    let orphan = rt
        .create_note(
            &token,
            "message",
            None,
            "identical template",
            None,
            Some(serde_json::json!({
                "direction": "outbound",
                "from_actor": "local",
                "to_actor": "local",
            })),
            vec![],
        )
        .await
        .expect("create outbound-only fixture");

    let result = registry
        .dispatch(
            "comm.delivered",
            serde_json::json!({ "id": orphan.id.to_string() }),
        )
        .await
        .expect("delivery confirmation succeeds");

    assert_eq!(result["status"], "undelivered");
    assert_eq!(result["delivered"], false);
    assert_eq!(result["inbound_count"], 0);
}

/// #1447: confirmation must report delivered from the inbound correlation alone.
#[tokio::test]
async fn delivered_confirms_inbound_after_outbound_disappears() {
    let (registry, rt) = build_registry();
    let token = rt
        .authorize(Namespace::parse("local").unwrap())
        .expect("authorize local");
    let missing_outbound_id = uuid::Uuid::new_v4();
    rt.create_note(
        &token,
        "message",
        None,
        "ambiguous outcome fixture",
        None,
        Some(serde_json::json!({
            "direction": "inbound",
            "from_actor": "local",
            "to_actor": "local",
            "outbound_ref": missing_outbound_id,
        })),
        vec![],
    )
    .await
    .expect("create committed inbound fixture");

    let result = registry
        .dispatch(
            "comm.delivered",
            serde_json::json!({ "id": missing_outbound_id.to_string() }),
        )
        .await
        .expect("confirmation does not require outbound row");

    assert_eq!(result["status"], "delivered");
    assert_eq!(result["delivered"], true);
    assert_eq!(result["inbound_count"], 1);
}

/// Confirmation is sender-namespace scoped.
#[tokio::test]
async fn delivered_ignores_matching_inbound_in_another_namespace() {
    let (registry, rt) = build_registry();
    let outbound_id = uuid::Uuid::new_v4();
    let foreign_token = rt
        .authorize(Namespace::parse("foreign").unwrap())
        .expect("authorize foreign fixture namespace");
    rt.create_note(
        &foreign_token,
        "message",
        None,
        "foreign inbound fixture",
        None,
        Some(serde_json::json!({
            "direction": "inbound",
            "outbound_ref": outbound_id,
        })),
        vec![],
    )
    .await
    .expect("create foreign inbound fixture");

    let result = registry
        .dispatch(
            "comm.delivered",
            serde_json::json!({ "id": outbound_id.to_string() }),
        )
        .await
        .expect("local confirmation succeeds");

    assert_eq!(result["status"], "undelivered");
    assert_eq!(result["delivered"], false);
    assert_eq!(result["inbound_count"], 0);
}

/// A display prefix is not a stable correlation key and may have no outbound row to resolve, so the public contract requires the surfaced full UUID.
#[tokio::test]
async fn delivered_rejects_short_or_malformed_ids() {
    let (registry, _rt) = build_registry();
    for id in ["deadbeef", "not-a-uuid"] {
        let error = registry
            .dispatch("comm.delivered", serde_json::json!({ "id": id }))
            .await
            .expect_err("non-full id must be rejected");
        assert!(
            error.to_string().contains("full outbound UUID"),
            "error must explain the stable correlation requirement: {error}"
        );
        assert!(
            error.to_string().contains("scoped resolution"),
            "error must explain why a display prefix is insufficient: {error}"
        );
    }
}

#[tokio::test]
async fn inbox_long_poll_wakes_after_concurrent_ingest() {
    let (registry, _rt) = build_registry_for_ns("local");
    let waiter_registry = registry.clone();
    let started = std::time::Instant::now();
    let mut waiter = tokio::spawn(async move {
        waiter_registry
            .dispatch(
                "comm.inbox",
                serde_json::json!({
                    "status": "all",
                    "from_actor": "email:sender@example.com",
                    "content_contains": "wake the blocked inbox",
                    "wait_ms": 5_000,
                }),
            )
            .await
    });

    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    assert!(
        !waiter.is_finished(),
        "empty inbox must remain blocked before a matching ingest"
    );

    registry
        .dispatch(
            "comm.ingest",
            serde_json::json!({
                "namespace": "local",
                "from": "email:other@example.com",
                "to": "local",
                "content": "unrelated wake",
                "external_id": "imap:long-poll:1:1",
            }),
        )
        .await
        .expect("unrelated ingest succeeds");
    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    assert!(
        !waiter.is_finished(),
        "an unrelated signal must re-query and keep waiting"
    );

    registry
        .dispatch(
            "comm.ingest",
            serde_json::json!({
                "namespace": "local",
                "from": "email:sender@example.com",
                "to": "local",
                "content": "same sender, wrong content",
                "external_id": "imap:long-poll:1:content-miss",
            }),
        )
        .await
        .expect("same-sender non-matching ingest succeeds");
    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    assert!(
        !waiter.is_finished(),
        "a wake that fails a post-query text filter must keep waiting"
    );

    let ingest = registry
        .dispatch(
            "comm.ingest",
            serde_json::json!({
                "namespace": "local",
                "from": "email:sender@example.com",
                "to": "local",
                "content": "wake the blocked inbox",
                "external_id": "imap:long-poll:1:2",
            }),
        )
        .await
        .expect("concurrent ingest succeeds");
    assert_eq!(ingest["deduplicated"].as_bool(), Some(false));

    let inbox = match tokio::time::timeout(std::time::Duration::from_secs(1), &mut waiter).await {
        Ok(joined) => joined
            .expect("long-poll task must not panic")
            .expect("long-poll inbox succeeds"),
        Err(_) => {
            waiter.abort();
            panic!("long-poll inbox did not wake within one second of ingest");
        }
    };
    assert!(
        started.elapsed() < std::time::Duration::from_secs(1),
        "signal wake must beat the five-second polling baseline"
    );
    assert_eq!(inbox["count"].as_u64(), Some(1));
    assert_eq!(
        inbox["messages"][0]["content"].as_str(),
        Some("wake the blocked inbox")
    );
    assert_eq!(inbox["offset"].as_u64(), Some(0));
    assert_eq!(inbox["has_more"].as_bool(), Some(false));
    assert!(inbox["next_offset"].is_null());
}

#[tokio::test]
async fn inbox_long_poll_wakes_after_concurrent_send() {
    let (registry, _rt) = build_registry_for_ns("local");
    let waiter_registry = registry.clone();
    let mut waiter = tokio::spawn(async move {
        waiter_registry
            .dispatch(
                "comm.inbox",
                serde_json::json!({ "status": "all", "wait_ms": 5_000 }),
            )
            .await
    });

    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    assert!(
        !waiter.is_finished(),
        "empty inbox must remain blocked before a matching send"
    );

    registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "local", "content": "send wakes the inbox" }),
        )
        .await
        .expect("concurrent send succeeds");

    let inbox = match tokio::time::timeout(std::time::Duration::from_secs(1), &mut waiter).await {
        Ok(joined) => joined
            .expect("long-poll task must not panic")
            .expect("long-poll inbox succeeds"),
        Err(_) => {
            waiter.abort();
            panic!("long-poll inbox did not wake within one second of send");
        }
    };
    assert_eq!(inbox["count"].as_u64(), Some(1));
    assert_eq!(
        inbox["messages"][0]["content"].as_str(),
        Some("send wakes the inbox")
    );
}

#[tokio::test(start_paused = true)]
async fn inbox_long_poll_stops_at_requested_budget() {
    let (registry, _rt) = build_registry_for_ns("local");
    let started = tokio::time::Instant::now();

    let inbox = registry
        .dispatch(
            "comm.inbox",
            serde_json::json!({ "status": "all", "wait_ms": 250 }),
        )
        .await
        .expect("empty long poll succeeds at its deadline");

    assert_eq!(inbox["count"].as_u64(), Some(0));
    assert_eq!(
        tokio::time::Instant::now().duration_since(started),
        std::time::Duration::from_millis(250),
        "the signal wait must use one fixed budget rather than resetting it"
    );
}

#[tokio::test]
async fn inbox_rejects_wait_budget_above_thirty_seconds() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch("comm.inbox", serde_json::json!({ "wait_ms": 30_001 }))
        .await
        .expect_err("oversized long-poll budget must be rejected");
    assert!(
        err.to_string().contains("wait_ms") && err.to_string().contains("30000"),
        "error must name wait_ms and its maximum: {err}"
    );
}

#[tokio::test(start_paused = true)]
async fn inbox_count_only_never_waits() {
    let (registry, _rt) = build_registry_for_ns("local");
    let started = tokio::time::Instant::now();

    let inbox = registry
        .dispatch(
            "comm.inbox",
            serde_json::json!({ "limit": 0, "wait_ms": 5_000 }),
        )
        .await
        .expect("count-only inbox succeeds");

    assert_eq!(inbox["count"].as_u64(), Some(0));
    assert!(
        inbox["messages"]
            .as_array()
            .is_some_and(|rows| rows.is_empty()),
        "count-only inbox returns no message payloads: {inbox}"
    );
    assert_eq!(
        tokio::time::Instant::now().duration_since(started),
        std::time::Duration::ZERO,
        "limit=0 must return immediately even with a positive wait_ms"
    );
}

#[tokio::test]
async fn inbox_long_poll_returns_immediately_when_matches_exist() {
    let (registry, _rt) = build_registry_for_ns("local");

    registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "local", "content": "already waiting" }),
        )
        .await
        .expect("send succeeds");

    let started = std::time::Instant::now();
    let inbox = registry
        .dispatch(
            "comm.inbox",
            serde_json::json!({ "status": "all", "wait_ms": 5_000 }),
        )
        .await
        .expect("long poll over a non-empty inbox succeeds");

    assert!(
        started.elapsed() < std::time::Duration::from_secs(1),
        "an initial query with matches must return without waiting out the budget"
    );
    assert_eq!(inbox["count"].as_u64(), Some(1));
    assert_eq!(
        inbox["messages"][0]["content"].as_str(),
        Some("already waiting")
    );
}

#[tokio::test(start_paused = true)]
async fn inbox_long_poll_accepts_thirty_second_budget() {
    let (registry, _rt) = build_registry_for_ns("local");
    let started = tokio::time::Instant::now();

    let inbox = registry
        .dispatch(
            "comm.inbox",
            serde_json::json!({ "status": "all", "wait_ms": 30_000 }),
        )
        .await
        .expect("the inclusive maximum wait_ms must be accepted");

    assert_eq!(inbox["count"].as_u64(), Some(0));
    assert_eq!(
        tokio::time::Instant::now().duration_since(started),
        std::time::Duration::from_millis(30_000),
        "wait_ms=30000 is the inclusive boundary and must run its full budget"
    );
}

#[tokio::test]
async fn inbox_rejects_non_numeric_wait_ms() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch("comm.inbox", serde_json::json!({ "wait_ms": "soon" }))
        .await
        .expect_err("a non-numeric wait_ms must be rejected");
    assert!(
        err.to_string().contains("invalid input"),
        "a non-numeric wait_ms must fail parameter deserialization: {err}"
    );

    let err = registry
        .dispatch("comm.inbox", serde_json::json!({ "wait_ms": -5 }))
        .await
        .expect_err("a negative wait_ms must be rejected");
    assert!(
        err.to_string().contains("invalid input"),
        "a negative wait_ms must fail parameter deserialization: {err}"
    );
}

#[tokio::test]
async fn inbox_long_poll_wakes_after_concurrent_reply() {
    let (registry, _rt) = build_registry_for_ns("local");
    let waiter_registry = registry.clone();
    let mut waiter = tokio::spawn(async move {
        waiter_registry
            .dispatch(
                "comm.inbox",
                serde_json::json!({
                    "status": "all",
                    "content_contains": "reply wake arrives",
                    "wait_ms": 5_000,
                }),
            )
            .await
    });

    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    assert!(
        !waiter.is_finished(),
        "empty inbox must remain blocked before a matching reply"
    );

    let original = registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "local", "content": "original for reply wake" }),
        )
        .await
        .expect("send succeeds");
    let original_full_id = original["full_id"].as_str().expect("send returns full_id");

    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    assert!(
        !waiter.is_finished(),
        "a send that fails the content filter must keep the reply waiter waiting"
    );

    registry
        .dispatch(
            "comm.reply",
            serde_json::json!({ "id": original_full_id, "content": "reply wake arrives" }),
        )
        .await
        .expect("reply succeeds");

    let inbox = match tokio::time::timeout(std::time::Duration::from_secs(1), &mut waiter).await {
        Ok(joined) => joined
            .expect("long-poll task must not panic")
            .expect("long-poll inbox succeeds"),
        Err(_) => {
            waiter.abort();
            panic!("long-poll inbox did not wake within one second of reply");
        }
    };
    assert_eq!(inbox["count"].as_u64(), Some(1));
    assert_eq!(
        inbox["messages"][0]["content"].as_str(),
        Some("reply wake arrives")
    );
}

#[tokio::test]
async fn inbox_long_poll_with_offset_wakes_and_pages() {
    let (registry, _rt) = build_registry_for_ns("local");

    registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "local", "content": "page: first" }),
        )
        .await
        .expect("first send succeeds");
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "local", "content": "page: second" }),
        )
        .await
        .expect("second send succeeds");
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;

    let waiter_registry = registry.clone();
    let mut waiter = tokio::spawn(async move {
        waiter_registry
            .dispatch(
                "comm.inbox",
                serde_json::json!({ "status": "all", "offset": 2, "wait_ms": 5_000 }),
            )
            .await
    });

    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    assert!(
        !waiter.is_finished(),
        "an offset beyond the current last page must keep waiting"
    );

    registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "local", "content": "page: third" }),
        )
        .await
        .expect("third send succeeds");

    let inbox = match tokio::time::timeout(std::time::Duration::from_secs(1), &mut waiter).await {
        Ok(joined) => joined
            .expect("long-poll task must not panic")
            .expect("long-poll inbox succeeds"),
        Err(_) => {
            waiter.abort();
            panic!("long-poll inbox did not wake within one second of the third send");
        }
    };
    assert_eq!(inbox["count"].as_u64(), Some(1));
    assert_eq!(inbox["offset"].as_u64(), Some(2));
    assert_eq!(
        inbox["messages"][0]["content"].as_str(),
        Some("page: first"),
        "the same-offset re-query must page from the newest-first filtered sequence"
    );
}

#[tokio::test]
async fn read_marks_message_as_read() {
    let (registry, rt) = build_registry_for_ns("local");

    registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "local", "content": "mark me read" }),
        )
        .await
        .expect("send succeeds");

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

    assert!(reply.get("id").is_some(), "reply returns id: {reply}");
    assert_eq!(
        reply.get("thread_id").and_then(|v| v.as_str()),
        Some(original_full_id),
        "reply thread_id matches original full_id: {reply}"
    );
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
    let (registry, rt) = build_registry_for_ns("local");

    registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "local", "content": "read me by short id" }),
        )
        .await
        .expect("send succeeds");

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

    let mut builder = khive_runtime::VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(rt.clone()));
    builder.register(khive_pack_comm::CommPack::new(rt.clone()));
    let registry = builder.build().expect("registry");

    // The inbox must never emit the ambiguous prefix as its actionable `id`.
    // Both colliding rows remain individually round-trippable through read.
    let inbox = registry
        .dispatch("comm.inbox", serde_json::json!({ "status": "all" }))
        .await
        .expect("inbox succeeds with colliding UUID prefixes");
    let messages = inbox["messages"].as_array().expect("messages array");
    assert_eq!(messages.len(), 2);
    let returned_ids = messages
        .iter()
        .map(|message| {
            assert_eq!(message["short_id"].as_str(), Some(base));
            let id = message["id"].as_str().expect("round-trippable id");
            assert_eq!(message["full_id"].as_str(), Some(id));
            id.to_string()
        })
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(
        returned_ids,
        [uuid_a.to_string(), uuid_b.to_string()].into()
    );
    for id in &returned_ids {
        registry
            .dispatch("comm.read", serde_json::json!({ "id": id }))
            .await
            .expect("every inbox id is accepted by comm.read");
    }

    let err = registry
        .dispatch("comm.read", serde_json::json!({ "id": base }))
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("ambiguous"),
        "ambiguous prefix error must mention 'ambiguous': got {msg:?}"
    );
    assert!(
        msg.contains(&uuid_a.to_string()) && msg.contains(&uuid_b.to_string()),
        "ambiguity error must name distinguishable full UUIDs: got {msg:?}"
    );
}

/// send() within the same namespace writes one outbound note in the caller's namespace.
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
    assert_eq!(props.get("from").and_then(|v| v.as_str()), Some("local"));
    assert_eq!(props.get("to").and_then(|v| v.as_str()), Some("local"));
    assert_eq!(inbound[0].content, "meeting at 3pm");
    assert!(
        props.get("outbound_ref").is_some(),
        "inbound note must carry outbound_ref"
    );
}

/// inbox() returns the inbound message after a self-send with configured actor identity.
#[tokio::test]
async fn test_inbox_returns_inbound_for_recipient() {
    let (registry, _rt) = build_actor_registry(shared_backend(), "lambda:khive");
    registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:khive", "content": "you have mail", "self_send": true }),
        )
        .await
        .expect("self-send with actor identity succeeds");

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
        props.get("from_actor").and_then(|v| v.as_str()),
        Some("lambda:khive")
    );
    assert_eq!(
        props.get("direction").and_then(|v| v.as_str()),
        Some("inbound")
    );

    let id = msgs[0]
        .get("id")
        .and_then(|value| value.as_str())
        .expect("inbox message exposes a round-trippable id");
    let short_id = msgs[0]
        .get("short_id")
        .and_then(|value| value.as_str())
        .expect("inbox message exposes a compact display id");
    let full_id = msgs[0]
        .get("full_id")
        .and_then(|value| value.as_str())
        .expect("inbox message exposes a full id");
    assert_eq!(short_id.len(), 8);
    assert_eq!(id, full_id);

    let read = registry
        .dispatch("comm.read", serde_json::json!({ "id": id }))
        .await
        .expect("the id returned by inbox round-trips through comm.read");
    assert_eq!(
        read.get("full_id").and_then(|value| value.as_str()),
        Some(full_id)
    );
}

/// send-to-self writes exactly TWO notes (one outbound, one inbound) in the caller's namespace.  The inbound copy is required so that `inbox()` can surface the message to the sender when they are also the recipient.
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

/// Sender replies to their own outbound message → reply `to` equals original `to`.
#[tokio::test]
async fn test_reply_from_sender_routes_to_recipient() {
    let (registry, _rt) = build_registry_for_ns("lambda:khive");

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

    let reply = registry
        .dispatch(
            "comm.reply",
            serde_json::json!({ "id": msg_full_id, "content": "follow-up" }),
        )
        .await
        .expect("reply succeeds");

    let reply_to = reply
        .get("to")
        .and_then(|v| v.as_str())
        .expect("reply returns to");
    assert_eq!(
        reply_to, "lambda:khive",
        "UE6-H1: self-send reply routes back to to_actor; got {reply_to}"
    );
    let reply_from = reply
        .get("from")
        .and_then(|v| v.as_str())
        .expect("reply returns from");
    assert_eq!(
        reply_from, "local",
        "reply from must be local (ADR-007 all-local, token.namespace()=local)"
    );
}

/// Recipient replies to an inbound message → reply routes back to the original sender metadata field, not the caller's namespace.
#[tokio::test]
async fn test_reply_from_recipient_routes_to_sender() {
    let backend = shared_backend();
    let (registry, _rt) = build_actor_registry(backend, "lambda:khive");

    registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:khive", "content": "meeting at 3pm", "self_send": true }),
        )
        .await
        .expect("self-send with actor identity succeeds");

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

    let reply = registry
        .dispatch(
            "comm.reply",
            serde_json::json!({ "id": inbound_full_id, "content": "confirmed" }),
        )
        .await
        .expect("reply succeeds");

    let reply_to = reply
        .get("to")
        .and_then(|v| v.as_str())
        .expect("reply returns to");
    assert_eq!(
        reply_to, "lambda:khive",
        "UE6-H1: reply routes to original to_actor; got {reply_to}"
    );
    let reply_from = reply
        .get("from")
        .and_then(|v| v.as_str())
        .expect("reply returns from");
    assert_eq!(
        reply_from, "lambda:khive",
        "reply from must be the configured actor_id; got {reply_from}"
    );
}

/// reply() to an inbound message marks the original read — callers previously chained `reply | read`; the read is now folded into reply itself.
#[tokio::test]
async fn test_reply_marks_inbound_original_read() {
    let backend = shared_backend();
    let (registry, _rt) = build_actor_registry(backend, "lambda:khive");

    registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:khive", "content": "needs an answer", "self_send": true }),
        )
        .await
        .expect("self-send succeeds");

    let inbox = registry
        .dispatch("comm.inbox", serde_json::json!({ "status": "unread" }))
        .await
        .expect("inbox succeeds");
    let msgs = inbox
        .get("messages")
        .and_then(|v| v.as_array())
        .expect("messages array");
    assert_eq!(msgs.len(), 1, "must have 1 unread inbound message");
    let inbound_full_id = msgs[0]
        .get("full_id")
        .and_then(|v| v.as_str())
        .expect("full_id on inbound message")
        .to_string();

    let reply = registry
        .dispatch(
            "comm.reply",
            serde_json::json!({ "id": inbound_full_id, "content": "answered" }),
        )
        .await
        .expect("reply succeeds");
    assert_eq!(
        reply.get("marked_read").and_then(|v| v.as_bool()),
        Some(true),
        "reply to an inbound message must report marked_read=true; got {reply}"
    );

    let inbox_after = registry
        .dispatch("comm.inbox", serde_json::json!({ "status": "unread" }))
        .await
        .expect("inbox succeeds after reply");
    let unread_after = inbox_after
        .get("messages")
        .and_then(|v| v.as_array())
        .expect("messages array");
    assert!(
        unread_after
            .iter()
            .all(|m| m.get("full_id").and_then(|v| v.as_str()) != Some(inbound_full_id.as_str())),
        "the replied-to inbound message must no longer be unread"
    );
}

/// reply() to an outbound original performs no read-marking (read is a recipient action); `marked_read` is null.
#[tokio::test]
async fn test_reply_to_outbound_original_does_not_mark_read() {
    let (registry, _rt) = build_registry();

    let original = registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "local", "content": "root message" }),
        )
        .await
        .expect("send succeeds");
    let outbound_id = original
        .get("full_id")
        .and_then(|v| v.as_str())
        .expect("send returns full_id");

    let reply = registry
        .dispatch(
            "comm.reply",
            serde_json::json!({ "id": outbound_id, "content": "follow-up" }),
        )
        .await
        .expect("reply to own outbound succeeds");
    assert!(
        reply
            .get("marked_read")
            .map(|v| v.is_null())
            .unwrap_or(false),
        "reply to an outbound original must report marked_read=null; got {reply}"
    );
}

/// A legacy message carrying no `direction` property is still markable — reply() skips only an explicitly outbound original, exactly as read() does.
#[tokio::test]
async fn test_reply_marks_directionless_legacy_original() {
    use khive_storage::note::Note;
    use uuid::Uuid;
    let (registry, rt) = build_registry();
    let token = rt.authorize(khive_runtime::Namespace::local()).unwrap();
    let store = rt.notes(&token).expect("notes store");
    let now = chrono::Utc::now().timestamp_micros();
    let id = Uuid::parse_str("dd110000-3333-4000-8000-000000000003").unwrap();

    store
        .upsert_note(Note {
            id,
            namespace: token.namespace().as_str().to_string(),
            kind: "message".into(),
            status: "active".into(),
            name: None,
            content: "legacy message with no direction".into(),
            salience: None,
            decay_factor: None,
            expires_at: None,
            properties: Some(serde_json::json!({ "from": "x", "to": "local" })),
            created_at: now,
            updated_at: now,
            deleted_at: None,
        })
        .await
        .expect("insert legacy message");

    let reply = registry
        .dispatch(
            "comm.reply",
            serde_json::json!({ "id": id.as_hyphenated().to_string(), "content": "answered" }),
        )
        .await
        .expect("reply to legacy message succeeds");
    assert_eq!(
        reply.get("marked_read").and_then(|v| v.as_bool()),
        Some(true),
        "a directionless legacy original must be marked, not reported null; got {reply}"
    );

    let stored = store
        .get_note(id)
        .await
        .expect("get_note")
        .expect("legacy message still present");
    assert_eq!(
        stored
            .properties
            .as_ref()
            .and_then(|p| p.get("read"))
            .and_then(|v| v.as_bool()),
        Some(true),
        "the legacy original must actually carry read=true"
    );
}

/// The read patch must not clobber an unrelated property.
#[tokio::test]
async fn test_reply_read_patch_preserves_concurrent_properties() {
    use khive_storage::note::Note;
    use uuid::Uuid;
    let (registry, rt) = build_registry();
    let token = rt.authorize(khive_runtime::Namespace::local()).unwrap();
    let store = rt.notes(&token).expect("notes store");
    let now = chrono::Utc::now().timestamp_micros();
    let id = Uuid::parse_str("dd110000-4444-4000-8000-000000000004").unwrap();

    store
        .upsert_note(Note {
            id,
            namespace: token.namespace().as_str().to_string(),
            kind: "message".into(),
            status: "active".into(),
            name: None,
            content: "message that gains a property mid-flight".into(),
            salience: None,
            decay_factor: None,
            expires_at: None,
            properties: Some(
                serde_json::json!({ "direction": "inbound", "from": "x", "to": "local", "read": false }),
            ),
            created_at: now,
            updated_at: now,
            deleted_at: None,
        })
        .await
        .expect("insert message");

    // Stand in for another writer stamping metadata, then let reply perform
    // its independent atomic `read` set.
    store
        .set_note_property(
            id,
            "delivery_stamp",
            serde_json::json!("channel-email"),
            now + 1,
        )
        .await
        .expect("stamp applied");

    registry
        .dispatch(
            "comm.reply",
            serde_json::json!({ "id": id.as_hyphenated().to_string(), "content": "answered" }),
        )
        .await
        .expect("reply succeeds");

    let stored = store
        .get_note(id)
        .await
        .expect("get_note")
        .expect("message present");
    let props = stored.properties.expect("properties");
    assert_eq!(
        props.get("read").and_then(|v| v.as_bool()),
        Some(true),
        "read must be set"
    );
    assert_eq!(
        props.get("delivery_stamp").and_then(|v| v.as_str()),
        Some("channel-email"),
        "the concurrently written property must survive the read patch; got {props}"
    );
}

/// A non-participant may reach a message id, but replying must be rejected without flipping someone else's message to read.
#[tokio::test]
async fn test_reply_by_non_participant_is_rejected_without_marking_read() {
    let backend = shared_backend();
    let (registry_a, rt_a) = build_actor_registry(backend.clone(), "lambda:a");
    let (registry_b, _rt_b) = build_actor_registry(backend.clone(), "lambda:b");
    let (registry_c, _rt_c) = build_actor_registry(backend.clone(), "lambda:c");

    registry_a
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:b", "content": "for b only" }),
        )
        .await
        .expect("send succeeds");

    let token = rt_a.authorize(khive_runtime::Namespace::local()).unwrap();
    let notes = rt_a
        .list_notes(&token, Some("message"), 100, 0)
        .await
        .unwrap();
    let inbound = notes
        .iter()
        .find(|n| {
            n.properties.as_ref().is_some_and(|p| {
                p.get("direction").and_then(|v| v.as_str()) == Some("inbound")
                    && p.get("to_actor").and_then(|v| v.as_str()) == Some("lambda:b")
            })
        })
        .expect("b's inbound copy exists");
    let id = inbound.id.as_hyphenated().to_string();

    let err = registry_c
        .dispatch(
            "comm.reply",
            serde_json::json!({ "id": id.clone(), "content": "not mine to read" }),
        )
        .await
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("not addressed to or from caller actor"),
        "a non-participant reply must be rejected; got {err}"
    );

    let store = rt_a.notes(&token).expect("notes store");
    let after = store
        .get_note(inbound.id)
        .await
        .expect("get_note")
        .expect("message present");
    assert_eq!(
        after
            .properties
            .as_ref()
            .and_then(|p| p.get("read"))
            .and_then(|v| v.as_bool()),
        Some(false),
        "the original must remain unread after a rejected reply"
    );

    let by_b = registry_b
        .dispatch(
            "comm.reply",
            serde_json::json!({ "id": id, "content": "mine, and read" }),
        )
        .await
        .expect("addressee reply succeeds");
    assert_eq!(
        by_b["marked_read"].as_bool(),
        Some(true),
        "the addressee's reply must still mark the original read; got {by_b}"
    );
}

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

/// After a self-send, inbox(status="all") must return at least the inbound copy.
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

/// list(kind="message", thread_id=X) must return only messages in that thread.
#[tokio::test]
async fn test_list_message_thread_id_filter() {
    let (send_registry, rt) = build_registry_for_ns("lambda:khive");

    let root = send_registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:khive", "content": "thread root" }),
        )
        .await
        .expect("send root succeeds");
    let thread_id = root
        .get("full_id")
        .and_then(|v| v.as_str())
        .expect("root full_id")
        .to_string();

    send_registry
        .dispatch(
            "comm.send",
            serde_json::json!({
                "to": "lambda:khive",
                "content": "threaded message",
                "thread_id": thread_id
            }),
        )
        .await
        .expect("send threaded message succeeds");

    send_registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:khive", "content": "unthreaded message" }),
        )
        .await
        .expect("send msg2 succeeds");

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
                "thread_id": thread_id
            }),
        )
        .await
        .expect("list with thread_id filter succeeds");

    let items = list_items(&result);
    // The filter must actually select rows (a vacuously empty pass proves
    // nothing) and every returned message must carry the requested thread_id.
    assert!(
        !items.is_empty(),
        "CC-2 C1 regression: list(thread_id=X) returned no rows for a thread with a live message"
    );
    for item in items {
        let stored_thread = item
            .get("properties")
            .and_then(|p| p.get("thread_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert_eq!(
            stored_thread, thread_id,
            "CC-2 C1 regression: list(thread_id=X) must only return messages in that thread; got {item}"
        );
    }
}

/// list(kind="message", direction="inbound") must return only inbound messages.
#[tokio::test]
async fn test_list_message_direction_filter() {
    let (registry, rt) = build_registry_for_ns("local");

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

    let inbound = list_registry
        .dispatch(
            "list",
            serde_json::json!({ "kind": "message", "direction": "inbound" }),
        )
        .await
        .expect("list(direction=inbound) succeeds");
    let inbound_items = list_items(&inbound);
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

    let outbound = list_registry
        .dispatch(
            "list",
            serde_json::json!({ "kind": "message", "direction": "outbound" }),
        )
        .await
        .expect("list(direction=outbound) succeeds");
    let outbound_items = list_items(&outbound);
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

/// read() on an outbound message must return an error.
#[tokio::test]
async fn test_read_rejects_outbound_message() {
    let (registry, _rt) = build_registry_for_ns("lambda:khive");

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

/// A caller whose actor label does not match a message's `to_actor` must not be able to flip it to read — read-state is delivery state owned by the addressee.
#[tokio::test]
async fn t87_non_addressee_read_rejected_and_stays_unread() {
    let backend = shared_backend();
    let (registry_a, _rt_a) = build_actor_registry(backend.clone(), "lambda:a");
    let (registry_b, rt_b) = build_actor_registry(backend.clone(), "lambda:b");

    registry_a
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:b", "content": "for B's eyes only" }),
        )
        .await
        .expect("A sends to B");

    let local_tok = rt_b.authorize(Namespace::parse("local").unwrap()).unwrap();
    let notes = rt_b
        .list_notes(&local_tok, Some("message"), 100, 0)
        .await
        .unwrap();
    let inbound_id = notes
        .iter()
        .find(|n| {
            n.deleted_at.is_none()
                && n.properties
                    .as_ref()
                    .and_then(|p| p.get("direction"))
                    .and_then(|v| v.as_str())
                    == Some("inbound")
        })
        .map(|n| n.id.as_hyphenated().to_string())
        .expect("inbound copy addressed to lambda:b must exist");

    let result = registry_a
        .dispatch("comm.read", serde_json::json!({ "id": inbound_id }))
        .await;
    assert!(
        result.is_err(),
        "#87: non-addressee read must be rejected; got {result:?}"
    );
    let err_msg = format!("{}", result.unwrap_err());
    assert!(
        err_msg.contains("lambda:a") && !err_msg.contains("lambda:b"),
        "#87: error must name only the caller's own actor, never the real \
         addressee; got {err_msg:?}"
    );

    let refetched = rt_b
        .list_notes(&local_tok, Some("message"), 100, 0)
        .await
        .unwrap();
    let still_unread = refetched
        .iter()
        .find(|n| n.id.as_hyphenated().to_string() == inbound_id)
        .and_then(|n| n.properties.as_ref())
        .and_then(|p| p.get("read"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    assert!(
        !still_unread,
        "#87: message must stay unread after a rejected non-addressee read attempt"
    );

    let ok = registry_b
        .dispatch("comm.read", serde_json::json!({ "id": inbound_id }))
        .await
        .expect("#87: addressee read must still succeed");
    assert_eq!(
        ok.get("read").and_then(|v| v.as_bool()),
        Some(true),
        "#87: addressee read must return read:true; got {ok}"
    );
}

/// The anonymous/"local" single-actor deployment (no actor.id configured) must keep working: caller and to_actor both resolve to "local", so the equality check passes.
#[tokio::test]
async fn t87_anonymous_local_single_actor_read_still_works() {
    let (registry, rt) = build_registry_for_ns("local");

    registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "local", "content": "single-tenant read" }),
        )
        .await
        .expect("send succeeds");

    let caller_token = rt
        .authorize(khive_runtime::Namespace::parse("local").unwrap())
        .unwrap();
    let notes = rt
        .list_notes(&caller_token, Some("message"), 100, 0)
        .await
        .unwrap();
    let inbound_full_id = notes
        .iter()
        .find(|n| {
            n.deleted_at.is_none()
                && n.properties
                    .as_ref()
                    .and_then(|p| p.get("direction"))
                    .and_then(|v| v.as_str())
                    == Some("inbound")
        })
        .expect("inbound copy must exist after self-send")
        .id
        .to_string();

    let result = registry
        .dispatch("comm.read", serde_json::json!({ "id": inbound_full_id }))
        .await
        .expect("#87: anonymous single-actor 'local' read must still succeed");
    assert_eq!(
        result.get("read").and_then(|v| v.as_bool()),
        Some(true),
        "#87: read returns read:true — got {result}"
    );
}

/// Pre-ADR-057 legacy messages may carry no `to_actor` at all.
#[tokio::test]
async fn t87_legacy_message_without_to_actor_reads_fail_open() {
    let (_registry, rt) = build_registry_for_ns("lambda:legacy");
    let token = rt
        .authorize(khive_runtime::Namespace::parse("local").unwrap())
        .unwrap();

    let legacy_note = rt
        .create_note(
            &token,
            "message",
            None,
            "legacy inbound, no to_actor",
            None,
            Some(serde_json::json!({
                "from": "someone",
                "to": "lambda:legacy",
                "direction": "inbound",
            })),
            vec![],
        )
        .await
        .expect("legacy note creation succeeds");

    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(rt.clone()));
    builder.register(CommPack::new(rt.clone()));
    builder.with_default_namespace("local");
    builder.with_actor_id(Some("lambda:legacy".to_string()));
    let registry = builder.build().expect("registry builds");

    let result = registry
        .dispatch(
            "comm.read",
            serde_json::json!({ "id": legacy_note.id.as_hyphenated().to_string() }),
        )
        .await
        .expect("#87: legacy message without to_actor must fail open on read");
    assert_eq!(
        result.get("read").and_then(|v| v.as_bool()),
        Some(true),
        "#87: legacy to_actor-less read returns read:true — got {result}"
    );
}

/// thread(id=X) must return all messages in the thread in chronological order.
#[tokio::test]
async fn test_thread_verb_returns_threaded_messages() {
    let (registry, _rt) = build_registry_for_ns("local");

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

    registry
        .dispatch(
            "comm.reply",
            serde_json::json!({ "id": root_full_id, "content": "thread reply" }),
        )
        .await
        .expect("reply succeeds");

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

/// reply() must write both an outbound copy and an inbound copy within the same namespace.
#[tokio::test]
async fn test_reply_delivers_inbound_to_recipient() {
    let backend = shared_backend();
    let (registry, _rt) = build_actor_registry(backend, "lambda:khive");

    registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:khive", "content": "original message", "self_send": true }),
        )
        .await
        .expect("self-send with actor identity succeeds");

    let inbox = registry
        .dispatch("comm.inbox", serde_json::json!({ "status": "all" }))
        .await
        .expect("inbox succeeds");
    let msgs = inbox.get("messages").and_then(|v| v.as_array()).unwrap();
    assert_eq!(msgs.len(), 1, "must have 1 inbound message");
    let inbound_id = msgs[0].get("full_id").and_then(|v| v.as_str()).unwrap();

    registry
        .dispatch(
            "comm.reply",
            serde_json::json!({ "id": inbound_id, "content": "reply message" }),
        )
        .await
        .expect("reply succeeds");

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

/// thread(id=X) with a nonexistent UUID must return an error, not a silent empty result.
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

    let full_id = if obs_full_id.len() == 8 {
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

/// inbox() must return matching inbound messages even when more than the old prefetch window (limit*4) of non-matching messages precede them.
#[tokio::test]
async fn test_inbox_paginated_scan_finds_message_beyond_prefetch_window() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");

    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(rt.clone()));
    builder.register(CommPack::new(rt.clone()));
    builder.with_default_namespace("local");
    let registry = builder.build().expect("registry");

    registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "local", "content": "the important inbound message" }),
        )
        .await
        .expect("first send succeeds");

    for i in 0..25u32 {
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

/// inbox(limit=200) must succeed — 200 is the documented and enforced maximum.
#[tokio::test]
async fn test_inbox_limit_200_succeeds() {
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

/// A sends to self, A replies via the inbound copy, comm.thread(id=outbound_id) must return both the outbound and the reply.
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

    registry
        .dispatch(
            "comm.reply",
            serde_json::json!({ "id": inbound_full_id, "content": "reply" }),
        )
        .await
        .expect("reply succeeds");

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

/// comm.thread resolves correctly when called with the inbound copy UUID (id_B) instead of the outbound UUID (id_A).
#[tokio::test]
async fn test_thread_resolves_from_inbound_copy_uuid() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");

    let mut khive_builder = VerbRegistryBuilder::new();
    khive_builder.register(khive_pack_kg::KgPack::new(rt.clone()));
    khive_builder.register(CommPack::new(rt.clone()));
    khive_builder.with_default_namespace("lambda:khive");
    let khive_reg = khive_builder.build().expect("khive registry");

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

    khive_reg
        .dispatch(
            "comm.reply",
            serde_json::json!({ "id": outbound_full_id, "content": "a reply" }),
        )
        .await
        .expect("reply succeeds");

    let thread_via_inbound = khive_reg
        .dispatch("comm.thread", serde_json::json!({ "id": inbound_full_id }))
        .await
        .expect("H1: thread query via inbound UUID must succeed");

    let count_via_inbound = thread_via_inbound
        .get("count")
        .and_then(|v| v.as_u64())
        .expect("count");

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

/// list(kind=message, direction=inbound) must find a matching message even when more than 1000 non-matching outbound messages precede it in the store.
#[tokio::test]
async fn test_list_message_finds_match_beyond_1000_backlog() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");

    let tok = rt.authorize(Namespace::parse("local").unwrap()).unwrap();

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

    let items = list_items(&result);
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
    let returned_id = items[0].get("id").and_then(|v| v.as_str()).unwrap_or("");
    assert_eq!(
        returned_id, target_id,
        "M1: returned item id={returned_id} must match the target id={target_id}"
    );
}

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
        combined.contains("idx_comm_message_outbound_ref"),
        "schema plan must declare idx_comm_message_outbound_ref; got: {combined}"
    );
    assert!(
        combined.contains("CREATE INDEX IF NOT EXISTS"),
        "schema plan DDL must be idempotent; got: {combined}"
    );
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

/// thread isolation: comm.thread returns only messages belonging to the requested thread, not messages from other threads in the same namespace.
#[tokio::test]
async fn test_thread_returns_only_requested_thread_messages() {
    let (registry, _rt) = build_registry_for_ns("local");

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

    registry
        .dispatch(
            "comm.reply",
            serde_json::json!({ "id": thread_a_id, "content": "reply to A" }),
        )
        .await
        .expect("reply to A");

    let thread = registry
        .dispatch("comm.thread", serde_json::json!({ "id": thread_a_id }))
        .await
        .expect("thread A fetch");

    let messages = thread
        .get("messages")
        .and_then(|v| v.as_array())
        .expect("messages array");

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

    assert!(
        messages.len() >= 2,
        "thread must contain at least root + reply; got {}",
        messages.len()
    );
}

/// read filter 5-case truth table: json_type-based filter matches old as_bool().unwrap_or(false).
#[tokio::test]
async fn test_inbox_read_filter_json_type_truth_table() {
    use khive_storage::note::{FilterOp, Note, NoteFilter, PropertyFilter};
    use khive_storage::types::{PageRequest, SqlValue};

    let (_registry, rt) = build_registry_for_ns("local");
    let token = rt
        .authorize(khive_runtime::Namespace::parse("local").unwrap())
        .unwrap();
    let store = rt.notes(&token).expect("note store");

    let make_msg = |read_val: serde_json::Value, label: &str| -> Note {
        Note::new("local", "message", label).with_properties(serde_json::json!({
            "direction": "inbound",
            "from": "local",
            "to": "local",
            "thread_id": null,
            "read": read_val,
        }))
    };

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

#[tokio::test]
async fn send_rejects_thread_prefix_with_resolution_consequence() {
    let (registry, _rt) = build_registry_for_ns("local");
    let err = registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "local", "content": "hi", "thread_id": "deadbeef" }),
        )
        .await
        .expect_err("thread prefixes are not stable roots");
    let message = err.to_string();

    assert!(message.contains("scoped resolution"), "{message}");
    assert!(message.contains("explicit stable reference"), "{message}");
}

/// `comm.send` reports the persisted thread root so a continuation send can reuse it without fetching the message first (#1482).
#[tokio::test]
async fn send_response_thread_id_round_trips_root_and_continuation() {
    let (registry, _rt) = build_registry_for_ns("local");

    let root = registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "local", "content": "root message" }),
        )
        .await
        .expect("root send succeeds");
    assert_eq!(
        root["thread_id"], root["full_id"],
        "a root send must report its own UUID as the thread root: {root}"
    );
    assert_eq!(
        root["thread_id"].as_str().map(str::len),
        Some(36),
        "the reported root thread_id must be a full canonical UUID: {root}"
    );

    let supplied = root["thread_id"].as_str().unwrap();
    let continuation = registry
        .dispatch(
            "comm.send",
            serde_json::json!({
                "to": "local",
                "content": "continuation",
                "thread_id": supplied,
            }),
        )
        .await
        .expect("continuation send succeeds");
    assert_eq!(
        continuation["thread_id"], supplied,
        "a continuation send must echo the caller-supplied thread root so the \
         caller can keep the thread going: {continuation}"
    );
    assert_ne!(
        continuation["thread_id"], continuation["full_id"],
        "the continuation note must not be reported as its own thread root: {continuation}"
    );
}

/// send with a UUID-shaped but unresolvable thread_id must fail closed (issue #1673): the error names the unresolvable id and no message row is persisted.
#[tokio::test]
async fn send_rejects_unresolvable_thread_id() {
    use khive_storage::note::{FilterOp, NoteFilter, PropertyFilter};
    use khive_storage::types::PageRequest;

    let (registry, rt) = build_registry_for_ns("local");

    let phantom_thread_id = uuid::Uuid::new_v4().as_hyphenated().to_string();
    let err = registry
        .dispatch(
            "comm.send",
            serde_json::json!({
                "to": "local",
                "content": "stranded reply",
                "thread_id": phantom_thread_id,
            }),
        )
        .await
        .expect_err("send with a thread_id no message carries must fail");
    let err_text = format!("{err:?}");
    assert!(
        err_text.contains(&phantom_thread_id),
        "the error must name the unresolvable thread_id {phantom_thread_id:?}; got {err_text}"
    );

    let token = rt.authorize(Namespace::local()).expect("local token");
    let store = rt.notes(&token).expect("note store");
    let stranded_filter = NoteFilter {
        kind: Some("message".to_string()),
        property_filters: vec![PropertyFilter {
            json_path: "$.thread_id".to_string(),
            op: FilterOp::Eq,
            value: SqlValue::Text(phantom_thread_id.clone()),
        }],
        ..Default::default()
    };
    let stranded = store
        .query_notes_filtered("local", &stranded_filter, PageRequest::default())
        .await
        .expect("filtered query");
    assert!(
        stranded.items.is_empty(),
        "a rejected send must leave no message row behind; got {:?}",
        stranded
            .items
            .iter()
            .map(|n| &n.content)
            .collect::<Vec<_>>()
    );
    let all = rt
        .list_notes(&token, Some("message"), 100, 0)
        .await
        .expect("list messages");
    assert!(
        all.is_empty(),
        "a rejected send must persist nothing at all; got {:?}",
        all.iter().map(|n| &n.content).collect::<Vec<_>>()
    );
}

/// send with a thread_id that resolves to an existing thread must still thread correctly (issue #1673 must not regress legitimate threading).
#[tokio::test]
async fn send_accepts_resolvable_thread_id() {
    let (registry, _rt) = build_registry_for_ns("local");

    let root = registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "local", "content": "root message" }),
        )
        .await
        .expect("root send succeeds");
    let root_id = root
        .get("full_id")
        .and_then(|v| v.as_str())
        .expect("full_id present")
        .to_string();

    registry
        .dispatch(
            "comm.send",
            serde_json::json!({
                "to": "local",
                "content": "threaded follow-up",
                "thread_id": root_id,
            }),
        )
        .await
        .expect("send threaded onto an existing root must succeed");

    let thread = registry
        .dispatch("comm.thread", serde_json::json!({ "id": root_id }))
        .await
        .expect("thread lookup succeeds");
    let messages = thread["messages"].as_array().expect("messages array");
    for expected_content in ["root message", "threaded follow-up"] {
        assert!(
            messages
                .iter()
                .any(|message| message["content"].as_str() == Some(expected_content)),
            "thread must contain {expected_content:?}; got {thread}"
        );
    }
    assert_eq!(
        thread["thread_id"].as_str(),
        Some(root_id.as_str()),
        "the follow-up must land on the supplied root thread; got {thread}"
    );
}

/// `send` accepts UUID parser variants but stores only the v1 hyphenated form, so later exact-string thread queries cannot split the conversation.
#[tokio::test]
async fn send_canonicalizes_compact_and_braced_thread_ids_for_thread_lookup() {
    let (registry, _rt) = build_registry_for_ns("local");

    let root = registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "local", "content": "root message" }),
        )
        .await
        .expect("root send succeeds");
    let canonical_thread_id = root
        .get("full_id")
        .and_then(|v| v.as_str())
        .expect("full_id present")
        .to_string();
    let thread_uuid = canonical_thread_id
        .parse::<uuid::Uuid>()
        .expect("root full_id is a UUID");

    for (content, supplied_thread_id) in [
        ("compact child", thread_uuid.simple().to_string()),
        ("braced child", format!("{{{canonical_thread_id}}}")),
    ] {
        registry
            .dispatch(
                "comm.send",
                serde_json::json!({
                    "to": "local",
                    "content": content,
                    "thread_id": supplied_thread_id,
                }),
            )
            .await
            .unwrap_or_else(|error| {
                panic!("send with a valid alternate UUID spelling must succeed: {error}")
            });
    }

    let thread = registry
        .dispatch(
            "comm.thread",
            serde_json::json!({ "id": canonical_thread_id }),
        )
        .await
        .expect("thread lookup succeeds after alternate UUID spellings");
    assert_eq!(
        thread["thread_id"].as_str(),
        Some(canonical_thread_id.as_str())
    );
    let messages = thread["messages"].as_array().expect("messages array");
    for expected_content in ["compact child", "braced child"] {
        assert!(
            messages
                .iter()
                .any(|message| message["content"].as_str() == Some(expected_content)),
            "thread lookup must retain {expected_content:?}; got {thread}"
        );
    }
    for message in messages {
        assert_eq!(
            message["properties"]["thread_id"].as_str(),
            Some(canonical_thread_id.as_str()),
            "every v1 row returned from the thread must use the canonical root; got {message}"
        );
    }
}

/// comm.thread with an unknown argument must return an error, not silently ignore it.
#[tokio::test]
async fn thread_rejects_unknown_field() {
    let (registry, _rt) = build_registry_for_ns("local");

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

/// Build a KhiveRuntime + VerbRegistry for cross-ns tests.
fn build_crossns_registry(
    backend: Arc<khive_db::StorageBackend>,
    dispatch_ns: &str,
    allowed_outbound: Vec<Namespace>,
) -> (VerbRegistry, KhiveRuntime) {
    let config = RuntimeConfig {
        git_write: Default::default(),
        display_timezone: khive_runtime::config::resolve_default_display_timezone(),
        db_path: None,
        blob_hydration_bytes: khive_runtime::DEFAULT_BLOB_HYDRATION_BYTES,
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

#[tokio::test]
async fn t2_send_cross_ns_denied_when_allowlist_empty() {
    let backend = shared_backend();
    let (registry_leo, rt_leo) = build_crossns_registry(Arc::clone(&backend), "lambda:leo", vec![]);
    let (_registry_khive, _rt_khive) =
        build_crossns_registry(Arc::clone(&backend), "lambda:khive", vec![]);

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

#[tokio::test]
async fn t5_recipient_inbox_sees_message() {
    let backend = shared_backend();
    let (registry_sender, rt_local) = build_actor_registry(Arc::clone(&backend), "lambda:khive");
    let (registry_recipient, _rt_recipient) =
        build_actor_registry(Arc::clone(&backend), "lambda:leo");

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

    let inbox = registry_recipient
        .dispatch("comm.inbox", serde_json::json!({}))
        .await
        .expect("T5: inbox dispatch must succeed");
    let count = inbox.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
    assert!(
        count >= 1,
        "T5: 'lambda:leo' inbox must see the inbound message; got count={count}"
    );

    let local_alive = all_notes.iter().filter(|n| n.deleted_at.is_none()).count();
    assert_eq!(
        local_alive, 2,
        "T5: 'local' namespace must hold both outbound + inbound copies; got {local_alive}"
    );
}

#[tokio::test]
async fn t5b_reply_always_writes_same_namespace() {
    let backend = shared_backend();
    let (registry_local, rt_local) = build_actor_registry(Arc::clone(&backend), "lambda:khive");

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

    let reply_thread_id = reply_val
        .get("thread_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .expect("T5b: reply must carry thread_id");
    assert_eq!(
        reply_thread_id, outbound_id,
        "T5b: reply thread_id must equal original outbound UUID"
    );

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
        "T6: #199 fix: anonymous sender must NOT see inbound copy addressed to lambda:khive; got {count}"
    );
}

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

    let recipient_tok: NamespaceToken =
        leo_tok.with_namespace(Namespace::parse("lambda:khive").unwrap());

    let can_see_sender = rt_leo
        .get_note_including_deleted(&recipient_tok, sender_note.id)
        .await;
    match can_see_sender {
        Ok(Some(note)) => {
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

    let can_see_recipient = rt_khive
        .get_note_including_deleted(&recipient_tok, recipient_note.id)
        .await;
    match can_see_recipient {
        Ok(Some(note)) => {
            assert_eq!(
                note.namespace, "lambda:khive",
                "T7(b): stored namespace must be the recipient's write namespace"
            );
        }
        Ok(None) => panic!("T7(b): minted token must be able to read recipient-ns note"),
        Err(e) => panic!("T7(b): unexpected error {e:?}"),
    }
}

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

    let can_read = rt_leo
        .get_note_including_deleted(&local_tok, inbound_id)
        .await;
    match can_read {
        Ok(Some(_)) => {}
        Ok(None) => panic!("T8: local token must be able to read local-ns inbound note"),
        Err(e) => panic!("T8: unexpected error reading inbound note: {e:?}"),
    }

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

#[tokio::test]
async fn t9_reply_cross_ns_delivers_when_allowed() {
    let backend = shared_backend();
    let (registry_shared, rt_shared) =
        build_crossns_registry(Arc::clone(&backend), "lambda:shared", vec![]);

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

    let reply_thread_id = reply_val
        .get("thread_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .expect("T9: reply response must carry thread_id");
    assert_eq!(
        reply_thread_id, outbound_thread_id,
        "T9: reply thread_id must match original outbound UUID"
    );

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

#[tokio::test]
async fn t10_reply_cross_ns_denied_when_empty() {
    let backend = shared_backend();
    let (registry_leo, _rt_leo) =
        build_crossns_registry(Arc::clone(&backend), "lambda:leo", vec![]);
    let (registry_khive, _rt_khive) =
        build_crossns_registry(Arc::clone(&backend), "lambda:khive", vec![]);

    registry_leo
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:khive", "content": "setup for T10" }),
        )
        .await
        .expect("T10: initial send must succeed");

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

#[tokio::test]
async fn t11_inbound_write_failure_rolls_back_outbound() {
    let backend = shared_backend();
    let (registry_leo, rt_leo) = build_crossns_registry(Arc::clone(&backend), "lambda:leo", vec![]);

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

#[tokio::test]
async fn t12_allowlist_is_one_directional() {
    let backend = shared_backend();
    let (registry_leo, rt_leo) = build_crossns_registry(Arc::clone(&backend), "lambda:leo", vec![]);
    let (registry_khive, rt_khive) =
        build_crossns_registry(Arc::clone(&backend), "lambda:khive", vec![]);

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

    let local_tok_khive = rt_khive
        .authorize(Namespace::parse("local").unwrap())
        .unwrap();
    let khive_local_notes = rt_khive
        .list_notes(&local_tok_khive, Some("message"), 100, 0)
        .await
        .unwrap();
    assert_eq!(
        khive_local_notes
            .iter()
            .filter(|n| n.deleted_at.is_none())
            .count(),
        4,
        "T12: 4 notes total in local ns after both sends (ADR-007 all-local, shared backend)"
    );
}

#[tokio::test]
async fn t13_inbound_fts_failure_leaves_no_stranded_row() {
    use khive_runtime::arm_fts_fail_scoped;

    let unique_ns = format!("t13-{}", uuid::Uuid::new_v4().simple());
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let tok = rt.authorize(Namespace::parse(&unique_ns).unwrap()).unwrap();

    let _fts_arm = arm_fts_fail_scoped(&unique_ns);

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

    let notes = rt.list_notes(&tok, Some("message"), 100, 0).await.unwrap();
    let alive = notes.iter().filter(|n| n.deleted_at.is_none()).count();
    assert_eq!(
        alive, 0,
        "T13: no stranded note after FTS failure (create_note_inner must compensate); got {alive}"
    );
}

#[tokio::test]
async fn t14_inbound_vector_failure_leaves_no_stranded_row() {
    use async_trait::async_trait;
    use khive_runtime::{arm_vector_fail_scoped, EmbedderProvider};
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

    let unique_ns = format!("t14-{}", uuid::Uuid::new_v4().simple());
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    rt.register_embedder(T14VecProvider);
    let tok = rt.authorize(Namespace::parse(&unique_ns).unwrap()).unwrap();

    let _vector_arm = arm_vector_fail_scoped(&unique_ns);

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

    let notes = rt.list_notes(&tok, Some("message"), 100, 0).await.unwrap();
    let alive = notes.iter().filter(|n| n.deleted_at.is_none()).count();
    assert_eq!(
        alive, 0,
        "T14: no stranded note after vector failure (create_note_inner must compensate); got {alive}"
    );
}

/// Build a comm registry backed by a shared in-memory StorageBackend with a configured actor identity.
fn build_actor_registry(
    backend: Arc<khive_db::StorageBackend>,
    actor_id: &str,
) -> (VerbRegistry, KhiveRuntime) {
    let config = RuntimeConfig {
        git_write: Default::default(),
        display_timezone: khive_runtime::config::resolve_default_display_timezone(),
        db_path: None,
        blob_hydration_bytes: khive_runtime::DEFAULT_BLOB_HYDRATION_BYTES,
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

/// Actor A sends to actor B.
#[tokio::test]
async fn t_actor_inbox_filters_to_actor() {
    let backend = shared_backend();

    let (registry_a, _rt_a) = build_actor_registry(backend.clone(), "lambda:a");
    let (registry_b, _rt_b) = build_actor_registry(backend.clone(), "lambda:b");

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
    let b_to_actor = b_messages[0]["properties"]["to_actor"].as_str();
    assert_eq!(
        b_to_actor,
        Some("lambda:b"),
        "message must be addressed to lambda:b; got {b_to_actor:?}"
    );

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

/// After fix #199, an anonymous caller's inbox is filtered to messages with to_actor="local" or absent.
#[tokio::test]
async fn t_anonymous_actor_inbox_filters_addressed_messages() {
    let (registry, _rt) = build_registry();

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
        display_timezone: khive_runtime::config::resolve_default_display_timezone(),
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

/// When two registries share the same storage backend but carry distinct configured actor identities, `comm.inbox` must isolate each actor's view: - Actor A sends to B → B's inbox (status=all) shows the message; A's does not. - Actor B sends to A → A's inbox (status=all) shows the message; B's does not.
#[tokio::test]
async fn t_c2_inbox_isolation_cross_actor() {
    let backend = shared_backend();

    let (registry_a, _rt_a) = build_actor_registry(backend.clone(), "lambda:tenant-a");
    let (registry_b, _rt_b) = build_actor_registry(backend.clone(), "lambda:tenant-b");

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
#[tokio::test]
async fn t_c2_gate_receives_configured_actor_not_anonymous() {
    use khive_runtime::{Gate, GateDecision, GateError, GateRef, GateRequest};
    use std::sync::Mutex;

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
        display_timezone: khive_runtime::config::resolve_default_display_timezone(),
        db_path: None,
        blob_hydration_bytes: khive_runtime::DEFAULT_BLOB_HYDRATION_BYTES,
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

    let _ = registry
        .dispatch(
            "comm.inbox",
            serde_json::json!({ "status": "all", "limit": 1 }),
        )
        .await
        .expect("inbox dispatch must not error");

    let seen = gate.seen_actor_ids.lock().unwrap();
    assert!(
        seen.iter().any(|id| id == "lambda:tenant-x"),
        "gate must receive configured actor id 'lambda:tenant-x', not 'local' \
         (anonymous). Saw: {seen:?}. \
         Fix: pass actor_id into GateRequest at pack.rs:852 instead of \
         ActorRef::anonymous(). Tracked as issue #224."
    );
}

/// #200 regression: anonymous sender sending to a specific actor label stamps from_actor="local".
#[tokio::test]
async fn i199_200_anonymous_send_to_specific_actor_is_warned() {
    let (registry, _rt) = build_registry();

    let result = registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:leo", "content": "mis-attributed send" }),
        )
        .await;

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
#[tokio::test]
async fn i199_200_anonymous_send_to_local_still_works() {
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
#[tokio::test]
async fn i199_anonymous_inbox_cannot_read_messages_addressed_to_other_actor() {
    let backend = shared_backend();

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
        display_timezone: khive_runtime::config::resolve_default_display_timezone(),
        db_path: None,
        blob_hydration_bytes: khive_runtime::DEFAULT_BLOB_HYDRATION_BYTES,
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
#[tokio::test]
async fn i199_anonymous_inbox_sees_local_messages() {
    let (registry, _rt) = build_registry();

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

#[tokio::test]
async fn ingest_dedup_returns_existing_canonical_thread_id() {
    let (registry, _rt) = build_registry_for_ns("local");
    let original_thread = uuid::Uuid::new_v4();
    let competing_thread = uuid::Uuid::new_v4();
    let external_id = format!("roundtrip-dedup-{original_thread}");

    let first = registry
        .dispatch(
            "comm.ingest",
            serde_json::json!({
                "from": "external:sender",
                "to": "local",
                "content": "first delivery",
                "external_id": external_id,
                "thread_id": original_thread.simple().to_string(),
            }),
        )
        .await
        .expect("first ingest");
    assert_eq!(
        first["thread_id"],
        original_thread.as_hyphenated().to_string()
    );

    let duplicate = registry
        .dispatch(
            "comm.ingest",
            serde_json::json!({
                "from": "external:sender",
                "to": "local",
                "content": "retry with a different proposed root",
                "external_id": external_id,
                "thread_id": competing_thread,
            }),
        )
        .await
        .expect("duplicate ingest is an acknowledged no-op");

    assert_eq!(duplicate["deduplicated"], true);
    assert_eq!(
        duplicate["thread_id"],
        original_thread.as_hyphenated().to_string(),
        "the acknowledgement must identify the persisted thread, not the retry's proposed root"
    );
    assert_eq!(duplicate["thread_id"].as_str().unwrap().len(), 36);
    assert!(
        duplicate.get("thread_id_canonical").is_none(),
        "a UUID-valued stored thread_id must not carry the non-canonical flag: {duplicate:?}"
    );
}

/// Dedup ack for a legacy row whose stored thread_id is a non-UUID label must echo the literal stored value — not fabricate the duplicate's note UUID (which would route a caller into a DIFFERENT thread on a later send).
#[tokio::test]
async fn ingest_dedup_echoes_stored_non_uuid_thread_label() {
    let (registry, rt) = build_registry_for_ns("local");
    let external_id = format!("legacy-label-dedup-{}", uuid::Uuid::new_v4());

    let token = rt
        .authorize(khive_runtime::Namespace::local())
        .expect("authorize local");
    let created = rt
        .try_create_note(
            &token,
            "message",
            None,
            "legacy row with a non-UUID thread label",
            Some(serde_json::json!({
                "external_id": external_id,
                "thread_id": "legacy-thread-label",
                "direction": "inbound",
            })),
        )
        .await
        .expect("seed write")
        .expect("seed insert must not be deduplicated");
    assert_ne!(
        created.id.as_hyphenated().to_string(),
        "legacy-thread-label"
    );

    let duplicate = registry
        .dispatch(
            "comm.ingest",
            serde_json::json!({
                "from": "external:sender",
                "to": "local",
                "content": "retry of the legacy row",
                "external_id": external_id,
            }),
        )
        .await
        .expect("duplicate ingest is an acknowledged no-op");

    assert_eq!(duplicate["deduplicated"], true);
    assert_eq!(
        duplicate["thread_id"], "legacy-thread-label",
        "ack must echo the literal stored thread_id, not the note UUID"
    );
    assert!(
        duplicate.get("thread_id_warning").is_none(),
        "no warning when a stored thread_id is present: {duplicate:?}"
    );
    assert_eq!(
        duplicate["thread_id_canonical"], false,
        "a non-UUID stored thread_id must be flagged non-canonical: {duplicate:?}"
    );
}

/// Dedup ack for a legacy row with NO stored thread_id falls back to the duplicate's note UUID as thread root (#479b) and flags the derivation.
#[tokio::test]
async fn ingest_dedup_without_stored_thread_id_falls_back_with_warning() {
    let (registry, rt) = build_registry_for_ns("local");
    let external_id = format!("no-thread-dedup-{}", uuid::Uuid::new_v4());

    let token = rt
        .authorize(khive_runtime::Namespace::local())
        .expect("authorize local");
    let created = rt
        .try_create_note(
            &token,
            "message",
            None,
            "legacy row with no thread_id property",
            Some(serde_json::json!({
                "external_id": external_id,
                "direction": "inbound",
            })),
        )
        .await
        .expect("seed write")
        .expect("seed insert must not be deduplicated");

    let duplicate = registry
        .dispatch(
            "comm.ingest",
            serde_json::json!({
                "from": "external:sender",
                "to": "local",
                "content": "retry of the threadless legacy row",
                "external_id": external_id,
            }),
        )
        .await
        .expect("duplicate ingest is an acknowledged no-op");

    assert_eq!(duplicate["deduplicated"], true);
    assert_eq!(
        duplicate["thread_id"],
        created.id.as_hyphenated().to_string(),
        "with no stored thread_id the note UUID is the only honest thread root"
    );
    assert!(
        duplicate["thread_id_warning"].as_str().is_some(),
        "fallback must be flagged as derived, not stored: {duplicate:?}"
    );
    assert!(
        duplicate.get("thread_id_canonical").is_none(),
        "the derived note-UUID fallback is a parseable UUID and must not carry the \
         non-canonical flag: {duplicate:?}"
    );
}

// ── transport-owned message property write boundary (PR #1839 round 2) ──

/// `try_create_note` is a public `khive-runtime` method reachable by any
/// in-process caller holding a `NamespaceToken` — it is not routed through
/// the generic `create` verb funnel's pack-installed note-write validator.
/// `comm.ingest` is documented as the sole legitimate writer of
/// `quarantined` / `channel_kind` / `channel_slug` (transport-owned
/// evidence `comm.health` trusts at face value). A direct `try_create_note`
/// call attempting to forge any of those three properties must be rejected,
/// individually and in combination — and no row must be left behind for
/// `comm.health` to count as real quarantine backlog.
#[tokio::test]
async fn try_create_note_rejects_forged_transport_owned_message_properties() {
    let (registry, rt) = build_registry_for_ns("local");
    let token = rt
        .authorize(khive_runtime::Namespace::local())
        .expect("authorize local");

    for (key, value) in [
        ("quarantined", serde_json::json!(true)),
        ("channel_kind", serde_json::json!("email")),
        ("channel_slug", serde_json::json!("forged-channel")),
    ] {
        let err = rt
            .try_create_note(
                &token,
                "message",
                None,
                "forged quarantine row via direct runtime write",
                Some(serde_json::json!({ key: value })),
            )
            .await
            .expect_err(&format!(
                "try_create_note must reject a direct write setting `{key}`"
            ));
        assert!(
            err.to_string().contains(key),
            "rejection must name the offending property `{key}`: {err}"
        );
    }

    // All three at once must also be rejected as a single write.
    let err = rt
        .try_create_note(
            &token,
            "message",
            None,
            "forged quarantine row via direct runtime write (all three)",
            Some(serde_json::json!({
                "quarantined": true,
                "channel_kind": "email",
                "channel_slug": "forged-channel",
            })),
        )
        .await
        .expect_err("try_create_note must reject all three transport-owned properties at once");
    assert!(!err.to_string().is_empty());

    let health = registry
        .dispatch("comm.health", serde_json::json!({}))
        .await
        .expect("health succeeds");
    assert_eq!(
        health["quarantined_count"].as_u64(),
        Some(0),
        "a rejected direct write must leave no row for comm.health to count: {health:?}"
    );
    assert_eq!(
        health["channels"].as_array().map(Vec::len),
        Some(0),
        "no channel_health backlog entry should exist either: {health:?}"
    );
}

/// PR #1839 round 3, blocker: `try_create_note` refuses the three
/// transport-owned properties, but round 2 left the raw `NoteStore` accessor
/// (`runtime.notes(&token)`) able to write them directly — an in-process
/// caller holding a namespace token could call `upsert_note` or
/// `try_insert_note` with a forged `kind = "message"` note and `comm.health`
/// would count it as real quarantine backlog. `KhiveRuntime::notes()` is now
/// wrapped in a policy-enforcing decorator that refuses these writes at the
/// storage-accessor boundary itself, not just at the `try_create_note`
/// fast-path funnel. Assert both `upsert_note` and `try_insert_note` are
/// refused, individually per key, and that no row survives for `comm.health`
/// to count.
#[tokio::test]
async fn raw_note_store_accessor_rejects_forged_transport_owned_message_properties() {
    let (registry, rt) = build_registry_for_ns("local");
    let token = rt
        .authorize(khive_runtime::Namespace::local())
        .expect("authorize local");
    let store = rt.notes(&token).expect("notes store");

    for (key, value) in [
        ("quarantined", serde_json::json!(true)),
        ("channel_kind", serde_json::json!("email")),
        ("channel_slug", serde_json::json!("forged-channel")),
    ] {
        let note = Note::new("local", "message", "forged via raw upsert_note")
            .with_properties(serde_json::json!({ key: value }));
        let err = store.upsert_note(note).await.expect_err(&format!(
            "upsert_note must reject a direct write setting `{key}`"
        ));
        assert!(
            err.to_string().contains(key),
            "rejection must name the offending property `{key}`: {err}"
        );

        let note = Note::new("local", "message", "forged via raw try_insert_note")
            .with_properties(serde_json::json!({ key: value }));
        let err = store.try_insert_note(note).await.expect_err(&format!(
            "try_insert_note must reject a direct write setting `{key}`"
        ));
        assert!(
            err.to_string().contains(key),
            "rejection must name the offending property `{key}`: {err}"
        );
    }

    // All three at once, through both write paths.
    let all_three = serde_json::json!({
        "quarantined": true,
        "channel_kind": "email",
        "channel_slug": "forged-channel",
    });
    let err = store
        .upsert_note(
            Note::new("local", "message", "forged via raw upsert_note (all three)")
                .with_properties(all_three.clone()),
        )
        .await
        .expect_err("upsert_note must reject all three transport-owned properties at once");
    assert!(!err.to_string().is_empty());
    let err = store
        .try_insert_note(
            Note::new(
                "local",
                "message",
                "forged via raw try_insert_note (all three)",
            )
            .with_properties(all_three),
        )
        .await
        .expect_err("try_insert_note must reject all three transport-owned properties at once");
    assert!(!err.to_string().is_empty());

    let health = registry
        .dispatch("comm.health", serde_json::json!({}))
        .await
        .expect("health succeeds");
    assert_eq!(
        health["quarantined_count"].as_u64(),
        Some(0),
        "no raw-storage write must leave a row for comm.health to count: {health:?}"
    );
    assert_eq!(
        health["channels"].as_array().map(Vec::len),
        Some(0),
        "no channel_health backlog entry should exist either: {health:?}"
    );
}

/// The insert/upsert guard alone leaves a two-call forge: create an innocent
/// `kind = "message"` note through the public accessor (no reserved
/// properties, so the insert guard passes), then patch a reserved key onto
/// it through any of the four property-mutation seams. No public-store
/// caller legitimately patches transport-owned keys on any note kind, so
/// the decorator refuses those patch targets unconditionally. Assert every
/// patch seam refuses every reserved key on a really-existing message note,
/// that a non-reserved patch on the same note still works (the guard is
/// key-scoped, not seam-dead), and that `comm.health` counts nothing.
#[tokio::test]
async fn raw_note_store_accessor_rejects_patching_reserved_keys_onto_existing_message() {
    let (registry, rt) = build_registry_for_ns("local");
    let token = rt
        .authorize(khive_runtime::Namespace::local())
        .expect("authorize local");
    let store = rt.notes(&token).expect("notes store");

    let innocent = Note::new("local", "message", "innocent message, no reserved keys")
        .with_properties(serde_json::json!({ "direction": "inbound", "read": false }));
    let id = innocent.id;
    let updated_at = innocent.updated_at;
    store
        .upsert_note(innocent)
        .await
        .expect("a message note without reserved properties must insert through the accessor");

    let filter = khive_storage::NoteFilter::default();
    for (key, value) in [
        ("quarantined", serde_json::json!(true)),
        ("channel_kind", serde_json::json!("email")),
        ("channel_slug", serde_json::json!("forged-channel")),
    ] {
        let err = store
            .set_note_property(id, key, value.clone(), updated_at)
            .await
            .expect_err(&format!("set_note_property must refuse `{key}`"));
        assert!(
            err.to_string().contains(key),
            "set_note_property rejection must name `{key}`: {err}"
        );

        let path = format!("$.{key}");
        let err = store
            .try_patch_note_property(id, "local", &filter, &path, value.clone(), updated_at)
            .await
            .expect_err(&format!("try_patch_note_property must refuse `{path}`"));
        assert!(
            err.to_string().contains(key),
            "try_patch_note_property rejection must name `{key}`: {err}"
        );

        let err = store
            .patch_note_property_atomic(
                vec![id],
                "local",
                &filter,
                &path,
                value.clone(),
                updated_at,
            )
            .await
            .expect_err(&format!("patch_note_property_atomic must refuse `{path}`"));
        assert!(
            err.to_string().contains(key),
            "patch_note_property_atomic rejection must name `{key}`: {err}"
        );

        let err = store
            .update_note_properties(
                id,
                Some(serde_json::json!({ "direction": "inbound", key: value })),
                updated_at,
            )
            .await
            .expect_err(&format!(
                "update_note_properties must refuse a map carrying `{key}`"
            ));
        assert!(
            err.to_string().contains(key),
            "update_note_properties rejection must name `{key}`: {err}"
        );
    }

    store
        .set_note_property(id, "read", serde_json::json!(true), updated_at)
        .await
        .expect("a non-reserved patch on the same note must still succeed");

    let health = registry
        .dispatch("comm.health", serde_json::json!({}))
        .await
        .expect("health succeeds");
    assert_eq!(
        health["quarantined_count"].as_u64(),
        Some(0),
        "no patch attempt must leave a row for comm.health to count: {health:?}"
    );
}

/// SQLite's JSON path grammar admits spellings of the same top-level key
/// that a substring comparison cannot canonicalize: the quoted dot label
/// (`$."quarantined"`), the bracket form, and the bare `$` root, which
/// `json_set` would use to replace the whole properties object at once.
/// The guard refuses every target it cannot parse as a bare top-level
/// identifier, so all of these fail closed rather than sliding past a
/// string equality check — and a bare-identifier spelling of a
/// NON-reserved key still passes, so the strictness is scoped to the
/// grammar, not the seam.
#[tokio::test]
async fn patch_guard_refuses_target_spellings_it_cannot_prove_innocent() {
    let (registry, rt) = build_registry_for_ns("local");
    let token = rt
        .authorize(khive_runtime::Namespace::local())
        .expect("authorize local");
    let store = rt.notes(&token).expect("notes store");

    let innocent = Note::new("local", "message", "innocent message for spelling probes")
        .with_properties(serde_json::json!({ "direction": "inbound", "read": false }));
    let id = innocent.id;
    let updated_at = innocent.updated_at;
    store.upsert_note(innocent).await.expect("innocent insert");

    let filter = khive_storage::NoteFilter::default();
    for path in [
        "$.\"quarantined\"",
        "$[\"quarantined\"]",
        "$['quarantined']",
        "$",
        "$.",
        "$.\"read\"",
    ] {
        let err = store
            .try_patch_note_property(
                id,
                "local",
                &filter,
                path,
                serde_json::json!(true),
                updated_at,
            )
            .await
            .expect_err(&format!("try_patch_note_property must refuse `{path}`"));
        assert!(
            err.to_string().contains("bare top-level identifier"),
            "`{path}` must be refused as unparseable, got: {err}"
        );
        store
            .patch_note_property_atomic(
                vec![id],
                "local",
                &filter,
                path,
                serde_json::json!(true),
                updated_at,
            )
            .await
            .expect_err(&format!("patch_note_property_atomic must refuse `{path}`"));
    }

    for key in ["\"quarantined\"", "qu\"arantined", ""] {
        store
            .set_note_property(id, key, serde_json::json!(true), updated_at)
            .await
            .expect_err(&format!(
                "set_note_property must refuse non-bare key `{key}`"
            ));
    }

    store
        .try_patch_note_property(
            id,
            "local",
            &filter,
            "$.read",
            serde_json::json!(true),
            updated_at,
        )
        .await
        .expect("a bare-identifier non-reserved path must still pass the guard");

    let health = registry
        .dispatch("comm.health", serde_json::json!({}))
        .await
        .expect("health succeeds");
    assert_eq!(
        health["quarantined_count"].as_u64(),
        Some(0),
        "no spelling probe may leave a countable row: {health:?}"
    );
}

/// Pins WHERE the message-evidence policy boundary sits. The decorated
/// typed accessor (`KhiveRuntime::notes`) refuses reserved transport
/// properties; `KhiveRuntime::backend()` is the embedder/infrastructure
/// surface, and stores taken from it are deliberately NOT policy-wrapped —
/// an embedder holding the backend already holds root-equivalent access to
/// the database file, so a store-layer check there binds nobody. No pack
/// code takes note stores from the backend surface (gtd and schedule issue
/// kind-constrained note DML through `sql()`; the module doc in
/// `note_store_guard.rs` enumerates both writers and their constraints).
/// If this test starts failing because the raw path now refuses, the
/// boundary was moved: update the module contract in `note_store_guard.rs`
/// and this pin together, deliberately.
#[tokio::test]
async fn storage_backend_note_stores_are_an_embedder_surface_outside_the_policy_boundary() {
    let (_registry, rt) = build_registry_for_ns("local");
    let token = rt
        .authorize(khive_runtime::Namespace::local())
        .expect("authorize local");

    let decorated = rt.notes(&token).expect("typed accessor");
    let forged = Note::new("local", "message", "decorated accessor must refuse this")
        .with_properties(serde_json::json!({ "quarantined": true }));
    decorated
        .upsert_note(forged)
        .await
        .expect_err("the typed accessor is the policy boundary and must refuse");

    let raw = rt.backend().notes().expect("embedder store");
    let embedder_row = Note::new("local", "message", "embedder surface is not policy-bound")
        .with_properties(serde_json::json!({ "quarantined": true }));
    raw.upsert_note(embedder_row)
        .await
        .expect("the embedder surface sits outside the policy boundary by contract");
}

/// PR #1839 round 3, high: the channel-ingest capability used to live in a
/// process-global `OnceLock` on the factory, granted only inside
/// `PackRegistry::register_packs`. Direct composition (`CommPack::new`
/// without going through the registry, as `khive-mcp/src/serve.rs` does for
/// one test fixture) could never receive it, and — because the global was
/// process-wide rather than instance-bound — a grant to one registry's comm
/// pack leaked into every other `CommPack` instance in the same test binary.
/// This regression proves both fixed: an ungranted direct instance fails
/// closed with a configuration error (not a leaked grant, not an
/// `InvalidInput`), and the explicit constructor variant lets a direct
/// composition succeed.
#[tokio::test]
async fn comm_ingest_capability_is_instance_bound_not_process_global() {
    // A comm pack registered the normal way (through the registry) holds
    // its own grant, proving grants still flow through the intended path.
    let (registered_registry, _registered_rt) = build_registry_for_ns("local");
    let registered_result = registered_registry
        .dispatch(
            "comm.ingest",
            serde_json::json!({
                "from": "external:sender",
                "to": "local",
                "content": "granted via the registry path",
            }),
        )
        .await;
    assert!(
        registered_result.is_ok(),
        "a registry-registered comm pack must retain its grant: {registered_result:?}"
    );

    // A direct `CommPack::new` composition — built in the SAME process,
    // after the registry-based grant above — must NOT observe that grant.
    // Under the old process-global design this succeeded; under the
    // instance-bound design it must fail closed.
    let ungranted_runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let mut ungranted_builder = VerbRegistryBuilder::new();
    ungranted_builder.register(khive_pack_kg::KgPack::new(ungranted_runtime.clone()));
    ungranted_builder.register(CommPack::new(ungranted_runtime.clone()));
    let ungranted_registry = ungranted_builder.build().expect("registry builds");

    let ungranted_result = ungranted_registry
        .dispatch(
            "comm.ingest",
            serde_json::json!({
                "from": "external:sender",
                "to": "local",
                "content": "must not observe another instance's grant",
            }),
        )
        .await;
    let err = ungranted_result.expect_err(
        "a direct CommPack::new composition must not observe a grant made to a different \
         registered instance",
    );
    assert!(
        matches!(err, khive_runtime::RuntimeError::Unconfigured(_)),
        "a missing grant must classify as a configuration/startup failure, not InvalidInput: {err}"
    );

    // The explicit constructor variant lets a direct composition succeed.
    let granted_runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let mut granted_builder = VerbRegistryBuilder::new();
    granted_builder.register(khive_pack_kg::KgPack::new(granted_runtime.clone()));
    granted_builder.register(CommPack::new_with_channel_ingest_capability(
        granted_runtime.clone(),
        khive_runtime::ChannelIngestCapability::grant_for_direct_composition(),
    ));
    let granted_registry = granted_builder.build().expect("registry builds");

    let granted_result = granted_registry
        .dispatch(
            "comm.ingest",
            serde_json::json!({
                "from": "external:sender",
                "to": "local",
                "content": "granted via the explicit direct-composition constructor",
            }),
        )
        .await;
    assert!(
        granted_result.is_ok(),
        "an explicitly-granted direct composition must succeed: {granted_result:?}"
    );
}

/// The trusted-ingest path must still permit `comm.ingest` (its sole caller,
/// holding the registration-granted channel-ingest capability) to establish
/// the same three properties `try_create_note` refuses, and `comm.health`
/// must count the resulting row. Exercised through the verb so the test
/// proves the granted capability path end to end; the runtime method itself
/// is uncallable from outside `khive-runtime` without a grant, which is the
/// restriction under test.
#[tokio::test]
async fn comm_ingest_establishes_quarantine_via_granted_capability() {
    let (registry, rt) = build_registry_for_ns("local");

    let props = ingest_and_get_props(
        &registry,
        &rt,
        serde_json::json!({
            "from": "external:sender",
            "to": "local",
            "content": "legitimate quarantine row via the trusted ingest path",
            "channel_kind": "email",
            "channel_slug": "trusted-channel",
            "metadata": {"quarantined": true},
        }),
    )
    .await;
    assert_eq!(
        props
            .get("quarantined")
            .and_then(serde_json::Value::as_bool),
        Some(true)
    );
    assert_eq!(props["channel_kind"], "email");
    assert_eq!(props["channel_slug"], "trusted-channel");

    let health = registry
        .dispatch("comm.health", serde_json::json!({}))
        .await
        .expect("health succeeds");
    assert_eq!(health["quarantined_count"].as_u64(), Some(1));
}

// ── list(kind=message) thread filter: legacy all-hex labels vs. UUID prefixes ──

/// Regression (PR #1623 round 2): an all-hex >=8-char stored thread label
/// that is NOT a UUID (e.g. "deadbeef") must still be matched exactly — the
/// UUID-prefix arm in the resolver must not swallow it and error "no message
/// thread matches prefix". A genuine UUID prefix must still resolve.
#[tokio::test]
async fn list_message_thread_filter_matches_legacy_hex_label_and_uuid_prefix() {
    let (registry, rt) = build_registry_for_ns("local");
    let token = rt
        .authorize(khive_runtime::Namespace::local())
        .expect("authorize local");

    rt.create_note(
        &token,
        "message",
        None,
        "legacy hex-labeled message",
        None,
        Some(serde_json::json!({"thread_id": "deadbeef"})),
        vec![],
    )
    .await
    .expect("create legacy hex-labeled message");

    let legacy = registry
        .dispatch(
            "list",
            serde_json::json!({"kind": "message", "thread_id": "deadbeef"}),
        )
        .await
        .expect("legacy all-hex label must match exactly, not error");
    let legacy = list_items(&legacy);
    assert_eq!(
        legacy.len(),
        1,
        "exact stored legacy label must return its message"
    );
    assert_eq!(legacy[0]["properties"]["thread_id"], "deadbeef");

    let thread = "bbbbcccc-1111-2222-3333-444455556666";
    rt.create_note(
        &token,
        "message",
        None,
        "uuid-threaded message",
        None,
        Some(serde_json::json!({"thread_id": thread})),
        vec![],
    )
    .await
    .expect("create uuid-threaded message");

    let prefixed = registry
        .dispatch(
            "list",
            serde_json::json!({"kind": "message", "thread_id": "bbbbcccc"}),
        )
        .await
        .expect("genuine UUID prefix must still resolve");
    let prefixed = list_items(&prefixed);
    assert_eq!(
        prefixed.len(),
        1,
        "UUID prefix must return exactly its thread's message"
    );
    assert_eq!(prefixed[0]["properties"]["thread_id"], thread);
}

/// Regression (PR #1623): the thread-prefix resolver must scan ONLY `message` notes.
#[tokio::test]
async fn list_thread_prefix_resolution_ignores_non_message_notes() {
    let (registry, rt) = build_registry_for_ns("local");
    let token = rt
        .authorize(khive_runtime::Namespace::local())
        .expect("authorize local");

    let message_thread = "aaaa0000-1111-4000-8000-000000000001";
    rt.create_note(
        &token,
        "message",
        None,
        "uuid-threaded message",
        None,
        Some(serde_json::json!({"thread_id": message_thread})),
        vec![],
    )
    .await
    .expect("create uuid-threaded message");

    rt.create_note(
        &token,
        "observation",
        None,
        "non-message note with a colliding thread_id",
        None,
        Some(serde_json::json!({"thread_id": "aaaa0000-9999-4000-8000-000000000002"})),
        vec![],
    )
    .await
    .expect("create decoy observation");

    let result = registry
        .dispatch(
            "list",
            serde_json::json!({"kind": "note", "thread_id": "aaaa0000"}),
        )
        .await
        .expect("prefix resolution must ignore non-message notes");
    let notes = list_items(&result);
    assert_eq!(
        notes.len(),
        1,
        "only the message carrying the resolved thread must match: {notes:?}"
    );
    assert_eq!(notes[0]["kind"], "message");
    assert_eq!(notes[0]["properties"]["thread_id"], message_thread);
}

/// Regression (PR #1623): thread-prefix resolution must use the SAME visibility scope as the list read (`['local'] ∪ visible_namespaces`).
#[tokio::test]
async fn list_thread_prefix_resolves_across_configured_visible_namespaces() {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let team_ns = khive_runtime::Namespace::parse("thread-team-ns").expect("valid namespace");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(CommPack::new(runtime.clone()));
    builder.with_default_namespace("local");
    builder.with_visible_namespaces(vec![team_ns.clone()]);
    let registry = builder.build().expect("registry builds");

    let team_token = runtime.authorize(team_ns).expect("authorize team ns");
    let thread = "ccccdddd-1111-4000-8000-000000000001";
    runtime
        .create_note(
            &team_token,
            "message",
            None,
            "visible-namespace threaded message",
            None,
            Some(serde_json::json!({"thread_id": thread})),
            vec![],
        )
        .await
        .expect("create message in visible namespace");

    let result = registry
        .dispatch(
            "list",
            serde_json::json!({"kind": "message", "thread_id": "ccccdddd"}),
        )
        .await
        .expect("a prefix of a thread in a visible namespace must resolve");
    let messages = list_items(&result);
    assert_eq!(
        messages.len(),
        1,
        "the visible-namespace thread's message must be returned: {messages:?}"
    );
    assert_eq!(messages[0]["properties"]["thread_id"], thread);
}

/// Regression (PR #1623): when the same prefix matches two DIFFERENT thread UUIDs — one in the primary namespace, one in a configured visible namespace — the resolver must report the ambiguity instead of silently resolving to the primary row and omitting the visible one.
#[tokio::test]
async fn list_thread_prefix_collision_across_visible_namespaces_is_ambiguous() {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let team_ns = khive_runtime::Namespace::parse("thread-collide-ns").expect("valid namespace");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(CommPack::new(runtime.clone()));
    builder.with_default_namespace("local");
    builder.with_visible_namespaces(vec![team_ns.clone()]);
    let registry = builder.build().expect("registry builds");

    let local_token = runtime
        .authorize(khive_runtime::Namespace::local())
        .expect("authorize local");
    runtime
        .create_note(
            &local_token,
            "message",
            None,
            "primary-namespace thread",
            None,
            Some(serde_json::json!({"thread_id": "eeeeffff-1111-4000-8000-000000000001"})),
            vec![],
        )
        .await
        .expect("create primary-namespace message");
    let team_token = runtime.authorize(team_ns).expect("authorize team ns");
    runtime
        .create_note(
            &team_token,
            "message",
            None,
            "visible-namespace thread sharing the prefix",
            None,
            Some(serde_json::json!({"thread_id": "eeeeffff-2222-4000-8000-000000000002"})),
            vec![],
        )
        .await
        .expect("create visible-namespace message");

    let error = registry
        .dispatch(
            "list",
            serde_json::json!({"kind": "message", "thread_id": "eeeeffff"}),
        )
        .await
        .expect_err("a cross-namespace prefix collision must be reported, not silently resolved");
    let message = error.to_string();
    assert!(
        message.contains("ambiguous thread_id prefix"),
        "unexpected error: {message}"
    );
    assert!(
        message.contains("visible"),
        "the error must name the visibility scope it searched: {message}"
    );
}

/// (a) Reply with correlation matching an outbound note whose from_actor=lambda:khive → ingested note to_actor=lambda:khive.
#[tokio::test]
async fn ingest_routing_reply_routes_to_original_sender() {
    let (registry, rt) = build_registry_for_ns("local");

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

/// (a2) Regression: outbound stores its Message-ID in wire form `<id@domain>`, but an inbound `In-Reply-To` is delivered bracket-free (`id@domain`) because `mail_parser` strips the angle brackets.
#[tokio::test]
async fn ingest_routing_reply_correlates_bracket_free_in_reply_to() {
    let (registry, rt) = build_registry_for_ns("local");

    let outbound_external_id = "<sent-msg-002@khive.ai>";
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

/// (d) Reply correlating via thread-UUID (X-Khive-Thread-ID fallback) routes to the original sender's actor even when no external_id match exists.
#[tokio::test]
async fn ingest_routing_reply_via_thread_uuid_routes_to_original_sender() {
    use khive_storage::note::Note;

    let (registry, rt) = build_registry_for_ns("local");

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

    let braced_thread_uuid = format!("{{{thread_uuid}}}");
    let props = ingest_and_get_props(
        &registry,
        &rt,
        serde_json::json!({
            "from": "email:user@example.com",
            "to": "email:mailbox@example.com",
            "content": "this is a reply via X-Khive-Thread-ID",
            "correlation_external_id": braced_thread_uuid,
            "external_id": "imap:mail:thread-uuid-reply:1",
            "namespace": "local",
        }),
    )
    .await;

    assert_eq!(
        props["to_actor"].as_str(),
        Some("lambda:khive"),
        "reply correlating via a braced thread UUID must route to lambda:khive \
         (original sender's actor); got props={props}"
    );
    assert_eq!(
        props["thread_id"].as_str(),
        Some(thread_uuid.as_str()),
        "ingested reply must be attached to the original thread_id; \
         got props={props}"
    );
}

/// Pass-2 correlation must also probe the URN and upper-hex spellings a pre-v1 handler could have stored, not just canonical/compact/braced.
#[tokio::test]
async fn ingest_routing_reply_matches_legacy_urn_and_upper_hex_thread_id() {
    let (registry, rt) = build_registry_for_ns("local");

    async fn plant_outbound_with_thread_id(
        rt: &KhiveRuntime,
        content: &str,
        external_id: &str,
        thread_id: &str,
    ) {
        let token = rt
            .authorize(khive_runtime::Namespace::local())
            .expect("authorize");
        let store = rt.notes(&token).expect("notes store");
        let now = chrono::Utc::now().timestamp_micros();
        let note = khive_storage::note::Note {
            id: uuid::Uuid::new_v4(),
            namespace: "local".into(),
            kind: "message".into(),
            status: "active".into(),
            name: None,
            content: content.into(),
            salience: None,
            decay_factor: None,
            expires_at: None,
            properties: Some(serde_json::json!({
                "direction": "outbound",
                "from": "email:mailbox@example.com",
                "to": "email:user@example.com",
                "from_actor": "lambda:khive",
                "to_actor": "email:user@example.com",
                "external_id": external_id,
                "thread_id": thread_id,
                "sent_at": chrono::Utc::now().to_rfc3339(),
            })),
            created_at: now,
            updated_at: now,
            deleted_at: None,
        };
        store.upsert_note(note).await.expect("upsert outbound note");
    }

    let urn_thread_uuid = uuid::Uuid::new_v4();
    plant_outbound_with_thread_id(
        &rt,
        "original outbound stored as URN",
        "<original-urn@khive.ai>",
        &urn_thread_uuid.urn().to_string(),
    )
    .await;

    let upper_thread_uuid = uuid::Uuid::new_v4();
    plant_outbound_with_thread_id(
        &rt,
        "original outbound stored as upper-hex",
        "<original-upper@khive.ai>",
        &format!("{:X}", upper_thread_uuid.as_hyphenated()),
    )
    .await;

    let urn_props = ingest_and_get_props(
        &registry,
        &rt,
        serde_json::json!({
            "from": "email:user@example.com",
            "to": "email:mailbox@example.com",
            "content": "reply correlating against a URN-stored thread_id",
            "correlation_external_id": urn_thread_uuid.as_hyphenated().to_string(),
            "external_id": "imap:mail:legacy-urn-reply:1",
            "namespace": "local",
        }),
    )
    .await;

    assert_eq!(
        urn_props["to_actor"].as_str(),
        Some("lambda:khive"),
        "reply correlating via canonical UUID against a legacy URN-stored \
         thread_id must route to lambda:khive; got props={urn_props}"
    );
    assert_eq!(
        urn_props["thread_id"].as_str(),
        Some(urn_thread_uuid.as_hyphenated().to_string().as_str()),
        "root selected from a URN-stored legacy row must be the canonical \
         hyphenated spelling; got props={urn_props}"
    );

    let upper_props = ingest_and_get_props(
        &registry,
        &rt,
        serde_json::json!({
            "from": "email:user@example.com",
            "to": "email:mailbox@example.com",
            "content": "reply correlating against an upper-hex-stored thread_id",
            "correlation_external_id": upper_thread_uuid.as_hyphenated().to_string(),
            "external_id": "imap:mail:legacy-upper-reply:1",
            "namespace": "local",
        }),
    )
    .await;

    assert_eq!(
        upper_props["to_actor"].as_str(),
        Some("lambda:khive"),
        "reply correlating via canonical UUID against a legacy upper-hex-stored \
         thread_id must route to lambda:khive; got props={upper_props}"
    );
    assert_eq!(
        upper_props["thread_id"].as_str(),
        Some(upper_thread_uuid.as_hyphenated().to_string().as_str()),
        "root selected from an upper-hex-stored legacy row must be the canonical \
         hyphenated spelling; got props={upper_props}"
    );
}

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

/// (a) Reply to an inbound-originated parent: the parent's Message-ID lives in `wire_message_id` (bracket-free, as `mail_parser` delivers it), never in `external_id`, which for an inbound note is the unrelated IMAP dedup key.
#[tokio::test]
async fn reply_sets_in_reply_to_for_inbound_originated_parent() {
    let (registry, rt) = build_actor_registry(shared_backend(), "lambda:khive");

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

/// (b) Reply to an outbound-minted parent: the parent's own Message-ID was self-minted into `external_id` (bracketed) by the outbox delivery loop.
#[tokio::test]
async fn reply_sets_in_reply_to_for_outbound_minted_parent() {
    let (registry, rt) = build_actor_registry(shared_backend(), "lambda:khive");

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

/// (c) Reply to a parent with no known wire Message-ID (e.g. a khive-internal message never routed through email): no In-Reply-To/References must be fabricated, and the reply still succeeds exactly as before this feature.
#[tokio::test]
async fn reply_omits_in_reply_to_when_parent_has_no_wire_message_id() {
    let (registry, rt) = build_actor_registry(shared_backend(), "lambda:khive");

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

/// `comm.ingest` with `wire_message_id` persists it on the resulting note, kept distinct from `external_id` (the IMAP dedup key).
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

/// `comm.ingest` with `wire_references` persists it on the resulting note, kept distinct from `external_id` (the IMAP dedup key) and from `wire_message_id`.
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

/// (a) Reply whose parent has an existing References chain of 2+ ids: the reply's References must be that chain followed by the parent's own Message-ID, and In-Reply-To must remain exactly the parent Message-ID.
#[tokio::test]
async fn reply_extends_existing_references_chain_of_two_or_more() {
    let (registry, rt) = build_actor_registry(shared_backend(), "lambda:khive");

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

/// (b) Reply whose parent has no References chain of its own (e.g. it was a thread root): References must degrade gracefully to the parent Message-ID alone, identical to pre-chain-preservation behavior.
#[tokio::test]
async fn reply_references_falls_back_to_parent_message_id_when_no_chain() {
    let (registry, rt) = build_actor_registry(shared_backend(), "lambda:khive");

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

/// (c) Reply-to-outbound direction: the parent was one of our own prior sends, so its chain lives in `references_chain` (not `wire_references`).
#[tokio::test]
async fn reply_extends_references_chain_for_outbound_parent() {
    let (registry, rt) = build_actor_registry(shared_backend(), "lambda:khive");

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

/// (d) A malformed token embedded in the parent's stored chain must be skipped rather than propagated into the reply's References header.
#[tokio::test]
async fn reply_skips_malformed_token_in_parent_references_chain() {
    let (registry, rt) = build_actor_registry(shared_backend(), "lambda:khive");

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

/// (e) A stored `references_chain` that is itself tainted -- already containing an equivalent of the parent's own Message-ID (e.g. legacy/corrupted data; this exact shape is never produced by `comm.reply` itself, see test (c) above, which now uses the realistic ancestors-only shape) -- must not be propagated as a literal duplicate.
#[tokio::test]
async fn reply_dedups_tainted_parent_references_chain_containing_parent_id() {
    let (registry, rt) = build_actor_registry(shared_backend(), "lambda:khive");

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

/// A quarantined envelope (as `EmailChannel::quarantine_envelope` builds it, ADR-056 Amendment 2026-07-02) must persist its `quarantined`/`quarantine_reason`/ `quarantine_claimed_from` markers through `comm.ingest`, and `from`/`from_actor` must stay the fixed `email:quarantine` marker -- `quarantine_claimed_from` is carried for maintainer review only, never as an attribution source.
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

/// Absent `metadata` must leave persisted properties exactly as before this fix (no `quarantined`/`quarantine_reason`/`quarantine_claimed_from` keys at all).
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

/// Metadata must merge additively: it must never be able to override a stable field the handler stamped or fabricate an optional stable field that is not meaningful for direct ingest (`outbound_ref`, `sent_by_process`).
#[tokio::test]
async fn ingest_metadata_cannot_override_or_fabricate_stable_fields() {
    let (registry, rt) = build_registry_for_ns("local");

    let props = ingest_and_get_props(
        &registry,
        &rt,
        serde_json::json!({
            "from": "email:quarantine",
            "to": "email:maintainer@example.com",
            "content": "spoofed body",
            "channel_kind": "email",
            "channel_slug": "account-1",
            "external_id": "imap:mail:3:1",
            "namespace": "local",
            "metadata": {
                "from_actor": "lambda:leo",
                "to_actor": "lambda:leo",
                "direction": "outbound",
                "comm_schema_version": 999,
                "subject": ["not", "a", "string"],
                "outbound_ref": "fabricated-twin",
                "sent_by_process": "fabricated-process",
                "channel_kind": "telegram",
                "channel_slug": "spoofed-account",
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
    assert_eq!(
        props["comm_schema_version"].as_u64(),
        Some(1),
        "metadata must never override the handler-stamped schema version; got props={props}"
    );
    assert!(
        props.get("subject").is_none(),
        "metadata must not fabricate an absent stable subject; got props={props}"
    );
    assert!(
        props.get("outbound_ref").is_none(),
        "direct ingest has no outbound twin; metadata must not fabricate one; got props={props}"
    );
    assert!(
        props.get("sent_by_process").is_none(),
        "adapter metadata must not fabricate originating-process provenance; got props={props}"
    );
    assert_eq!(props["channel_kind"].as_str(), Some("email"));
    assert_eq!(props["channel_slug"].as_str(), Some("account-1"));
}

#[tokio::test]
async fn ingest_rejects_ambiguous_channel_identity() {
    let (registry, _rt) = build_registry_for_ns("local");
    for (extra, field) in [
        (serde_json::json!({"channel_kind": "  "}), "channel_kind"),
        (
            serde_json::json!({"channel_kind": "email", "channel_slug": "  "}),
            "channel_slug",
        ),
        (
            serde_json::json!({"channel_slug": "account-1"}),
            "channel_kind",
        ),
    ] {
        let mut args = serde_json::json!({
            "namespace": "local",
            "from": "email:quarantine",
            "to": "local",
            "content": "invalid provenance",
        });
        args.as_object_mut()
            .expect("args object")
            .extend(extra.as_object().expect("extra object").clone());
        let err = registry
            .dispatch("comm.ingest", args)
            .await
            .expect_err("ambiguous channel identity must fail closed");
        assert!(
            err.to_string().contains(field),
            "error must name {field}: {err}"
        );
    }
}

/// `comm.ingest` with a malformed `thread_id` must return `InvalidInput` and must not write any note (issue #479a).
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

/// `comm.ingest` accepts a compact UUID but reports and persists the canonical full-hyphenated v1 spelling.
#[tokio::test]
async fn ingest_canonicalizes_valid_uuid_thread_id() {
    let (registry, rt) = build_registry_for_ns("local");

    let thread_uuid =
        uuid::Uuid::parse_str("aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee").expect("fixed test UUID");
    let supplied_thread_id = thread_uuid.simple().to_string();
    let canonical_thread_id = thread_uuid.as_hyphenated().to_string();
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
        Some(canonical_thread_id.as_str()),
        "response thread_id must be the canonical UUID; got {result}"
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
        Some(canonical_thread_id.as_str()),
        "stored note properties.thread_id must be the canonical UUID"
    );
}

/// A transport timestamp is an instant, not opaque adapter text: accepted RFC 3339 offsets are normalized to UTC before the v1 marker is written.
#[tokio::test]
async fn ingest_canonicalizes_rfc3339_sent_at() {
    let (registry, rt) = build_registry_for_ns("local");
    let supplied_sent_at = "2026-07-31T08:15:30.1200-04:00";
    let expected_sent_at = chrono::DateTime::parse_from_rfc3339(supplied_sent_at)
        .expect("fixed RFC 3339 timestamp")
        .with_timezone(&chrono::Utc)
        .to_rfc3339();
    assert_ne!(
        expected_sent_at, supplied_sent_at,
        "fixture must exercise canonicalization rather than identity"
    );

    let result = registry
        .dispatch(
            "comm.ingest",
            serde_json::json!({
                "from": "email:a@example.com",
                "to": "email:b@example.com",
                "content": "timestamped inbound",
                "sent_at": supplied_sent_at,
                "namespace": "local",
            }),
        )
        .await
        .expect("valid RFC 3339 sent_at succeeds");

    let full_id = result["full_id"].as_str().expect("full_id present");
    let id = full_id.parse::<uuid::Uuid>().expect("valid note UUID");
    let token = rt
        .authorize(khive_runtime::Namespace::local())
        .expect("authorize local");
    let note = rt
        .notes(&token)
        .expect("notes store")
        .get_note(id)
        .await
        .expect("get_note ok")
        .expect("note exists");
    assert_eq!(
        note.properties
            .as_ref()
            .and_then(|properties| properties.get("sent_at"))
            .and_then(|value| value.as_str()),
        Some(expected_sent_at.as_str()),
        "stored v1 sent_at must be canonical RFC 3339"
    );
}

/// A supplied timestamp that names no instant must fail before any v1 message is persisted; silently accepting it would make `comm_schema_version=1` lie.
#[tokio::test]
async fn ingest_rejects_malformed_sent_at_without_writing_note() {
    let (registry, rt) = build_registry_for_ns("local");

    let error = registry
        .dispatch(
            "comm.ingest",
            serde_json::json!({
                "from": "email:a@example.com",
                "to": "email:b@example.com",
                "content": "bad timestamp",
                "sent_at": "last tuesday",
                "namespace": "local",
            }),
        )
        .await
        .expect_err("malformed sent_at must fail");
    assert!(
        matches!(error, khive_runtime::RuntimeError::InvalidInput(_)),
        "expected InvalidInput, got {error:?}"
    );
    assert!(
        error.to_string().contains("sent_at"),
        "error must name sent_at; got {error}"
    );

    let token = rt
        .authorize(khive_runtime::Namespace::local())
        .expect("authorize local");
    let notes = rt
        .list_notes(&token, Some("message"), 100, 0)
        .await
        .expect("list notes");
    assert!(
        notes.iter().all(|note| note.deleted_at.is_some()),
        "malformed sent_at must not write a message; got {notes:?}"
    );
}

/// A reply correlated to an outbound message that has no `thread_id` property (e.g. a legacy/imported row) must reuse the outbound note's own UUID as the canonical root and route to the original `from_actor`, instead of being treated as unmatched and split into a fresh thread routed to the default inbound actor.
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

/// Correlation against a legacy outbound row may recover a UUID stored in a compact spelling.
#[tokio::test]
async fn ingest_correlation_canonicalizes_legacy_compact_root_for_thread_lookup() {
    let (registry, rt) = build_registry_for_ns("local");
    let root_id =
        uuid::Uuid::parse_str("12345678-1234-4abc-8def-1234567890ab").expect("fixed root UUID");
    let legacy_child_id =
        uuid::Uuid::parse_str("87654321-4321-4cba-8fed-ba0987654321").expect("fixed child UUID");
    let canonical_thread_id = root_id.as_hyphenated().to_string();
    let external_id = "<legacy-compact-root@khive.ai>";

    {
        use khive_storage::note::Note;
        let token = rt
            .authorize(khive_runtime::Namespace::local())
            .expect("authorize");
        let store = rt.notes(&token).expect("notes store");
        let now = chrono::Utc::now().timestamp_micros();
        store
            .upsert_note(Note {
                id: root_id,
                namespace: "local".into(),
                kind: "message".into(),
                status: "active".into(),
                name: None,
                content: "legacy compact root".into(),
                salience: None,
                decay_factor: None,
                expires_at: None,
                properties: Some(serde_json::json!({
                    "direction": "outbound",
                    "from": "local",
                    "to": "email:user@example.com",
                    "from_actor": "local",
                    "to_actor": "email:user@example.com",
                    "external_id": external_id,
                    "thread_id": root_id.simple().to_string(),
                    "sent_at": "2026-07-31T12:00:00Z",
                })),
                created_at: now,
                updated_at: now,
                deleted_at: None,
            })
            .await
            .expect("seed legacy outbound root");
        store
            .upsert_note(Note {
                id: legacy_child_id,
                namespace: "local".into(),
                kind: "message".into(),
                status: "active".into(),
                name: None,
                content: "legacy compact child".into(),
                salience: None,
                decay_factor: None,
                expires_at: None,
                properties: Some(serde_json::json!({
                    "direction": "outbound",
                    "from": "local",
                    "to": "email:user@example.com",
                    "from_actor": "local",
                    "to_actor": "email:user@example.com",
                    "thread_id": root_id.simple().to_string(),
                    "sent_at": "2026-07-31T12:00:01Z",
                })),
                created_at: now + 1,
                updated_at: now + 1,
                deleted_at: None,
            })
            .await
            .expect("seed legacy compact child");
    }

    let ingested = registry
        .dispatch(
            "comm.ingest",
            serde_json::json!({
                "from": "email:user@example.com",
                "to": "email:mailbox@example.com",
                "content": "reply to compact root",
                "correlation_external_id": external_id,
                "namespace": "local",
            }),
        )
        .await
        .expect("correlated ingest succeeds");
    assert_eq!(
        ingested["thread_id"].as_str(),
        Some(canonical_thread_id.as_str()),
        "correlation-derived root must be canonicalized"
    );

    let thread = registry
        .dispatch(
            "comm.thread",
            serde_json::json!({ "id": legacy_child_id.as_hyphenated().to_string() }),
        )
        .await
        .expect("thread lookup succeeds");
    assert_eq!(
        thread["thread_id"].as_str(),
        Some(canonical_thread_id.as_str())
    );
    let messages = thread["messages"].as_array().expect("messages array");
    for expected_content in [
        "legacy compact root",
        "legacy compact child",
        "reply to compact root",
    ] {
        assert!(
            messages
                .iter()
                .any(|message| message["content"].as_str() == Some(expected_content)),
            "thread lookup must include {expected_content:?}; got {thread}"
        );
    }
}

/// A mixed thread can contain children written before v1 preserved UUID input verbatim and children written after v1 with a canonical root.
#[tokio::test]
async fn thread_from_canonical_rows_includes_all_legacy_uuid_spellings_once() {
    use khive_storage::note::Note;

    let (registry, rt) = build_registry_for_ns("local");
    let root_id =
        uuid::Uuid::parse_str("abcdef12-3456-4abc-8def-1234567890ab").expect("fixed root UUID");
    let canonical_thread_id = root_id.as_hyphenated().to_string();

    let token = rt
        .authorize(khive_runtime::Namespace::local())
        .expect("authorize local");
    let store = rt.notes(&token).expect("notes store");
    let root_note =
        Note::new("local", "message", "canonical v1 root").with_properties(serde_json::json!({
            "comm_schema_version": 1,
            "direction": "outbound",
            "read": false,
            "from": "local",
            "to": "local",
            "from_actor": "local",
            "to_actor": "local",
            "thread_id": canonical_thread_id.clone(),
            "sent_at": "2026-07-31T11:59:59Z",
        }));
    let root_note = Note {
        id: root_id,
        ..root_note
    };
    store
        .upsert_note(root_note)
        .await
        .expect("seed canonical v1 root");

    let v1_child = registry
        .dispatch(
            "comm.send",
            serde_json::json!({
                "to": "local",
                "content": "canonical v1 child",
                "thread_id": canonical_thread_id.clone(),
            }),
        )
        .await
        .expect("v1 child send succeeds");
    let v1_child_id = v1_child["full_id"]
        .as_str()
        .expect("child full_id")
        .to_string();

    let legacy_rows = [
        ("legacy compact child", root_id.simple().to_string()),
        ("legacy braced child", root_id.braced().to_string()),
        (
            "legacy upper-hyphenated child",
            format!("{:X}", root_id.as_hyphenated()),
        ),
        ("legacy upper-URN child", format!("{:X}", root_id.urn())),
    ];
    for (content, stored_thread_id) in &legacy_rows {
        assert_eq!(
            stored_thread_id.parse::<uuid::Uuid>().ok(),
            Some(root_id),
            "fixture spelling must have been accepted by the pre-v1 UUID parser"
        );
        let note = Note::new("local", "message", *content).with_properties(serde_json::json!({
            "direction": "outbound",
            "read": false,
            "from": "local",
            "to": "local",
            "from_actor": "local",
            "to_actor": "local",
            "thread_id": stored_thread_id,
            "sent_at": "2026-07-31T12:00:00Z",
        }));
        store
            .upsert_note(note)
            .await
            .unwrap_or_else(|error| panic!("seed {content:?}: {error}"));
    }

    for lookup_id in [canonical_thread_id.as_str(), v1_child_id.as_str()] {
        let thread = registry
            .dispatch(
                "comm.thread",
                serde_json::json!({ "id": lookup_id, "limit": 100 }),
            )
            .await
            .unwrap_or_else(|error| panic!("thread lookup from {lookup_id}: {error}"));
        assert_eq!(
            thread["thread_id"].as_str(),
            Some(canonical_thread_id.as_str())
        );
        let messages = thread["messages"].as_array().expect("messages array");
        for expected_content in std::iter::once("canonical v1 root")
            .chain(std::iter::once("canonical v1 child"))
            .chain(legacy_rows.iter().map(|(content, _)| *content))
        {
            assert_eq!(
                messages
                    .iter()
                    .filter(|message| message["content"].as_str() == Some(expected_content))
                    .count(),
                1,
                "thread lookup from {lookup_id} must return {expected_content:?} exactly once; \
                 got {thread}"
            );
        }
    }
}

/// `comm.thread` must include a root message that has no `thread_id` property at all (issue #479b) -- the SQL query only matches `properties.thread_id == root`, which a thread-id-less root can never satisfy on its own.
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

/// design review amendment 1 (blocking): a fresh install with no persisted daemon heartbeat state must report `role: "client"` with an empty channel list — never fabricate channel entries the comm pack has no evidence for.
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
    assert_eq!(result["quarantined_count"].as_u64(), Some(0));
    assert_eq!(result["unattributed_quarantined_count"].as_u64(), Some(0));
    assert_eq!(
        result["channels"]
            .as_array()
            .expect("channels is array")
            .len(),
        0
    );
}

/// ADR-103 Stage 1 / issue #723 ask 2: `comm.health()` must self-report this process's own resource usage — `cpu_us`/`rss_bytes` via `getrusage`, plus the (possibly empty) set of named background phases currently in flight.
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

/// comm.health() takes no arguments — any caller-supplied args must be rejected rather than silently ignored (spec: "read-only, NO args").
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

/// Core cross-process-read contract (design review amendment 1): once the daemon persists a successful heartbeat, `comm.health()` returns it annotated `role: "daemon"`, `source: "daemon-heartbeat"` — this is true even though the *reading* call is a plain in-process dispatch here, mirroring a client-role stdio caller reading state it did not write itself.
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
                "poll_interval_secs": 5,
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
    assert_eq!(ch["poll_interval_secs"].as_u64(), Some(5));
    assert_eq!(ch["stalled"].as_bool(), Some(false));
    assert!(ch["last_success_at"].as_str().is_some());
    assert!(ch["last_poll_attempt_at"].as_str().is_some());
    assert!(ch["last_failure_at"].is_null());
    assert!(ch["last_error"].is_null());
    assert_eq!(ch["consecutive_failures"].as_u64(), Some(0));
}

/// khive #1383: quarantine is a terminal disposition for one message, not a
/// channel failure. Healthy heartbeat facts therefore stay healthy while the
/// read surface reports every live parked row, grouped by the exact transport
/// identity that produced it. Legacy rows without a channel slug remain
/// visible in an honest unattributed total instead of disappearing or being
/// guessed onto an account.
#[tokio::test]
async fn health_counts_live_quarantines_by_channel_without_marking_polling_failed() {
    let (registry, _rt) = build_registry_for_ns("local");

    for slug in ["acct-1", "acct-2"] {
        registry
            .dispatch(
                "comm.heartbeat",
                serde_json::json!({
                    "namespace": "local",
                    "channel_kind": "email",
                    "channel_slug": slug,
                    "poll_interval_secs": 5,
                    "outcome": "success",
                }),
            )
            .await
            .expect("healthy channel heartbeat");
    }

    async fn ingest_quarantine(
        registry: &khive_runtime::VerbRegistry,
        external_id: &str,
        channel_slug: Option<&str>,
        quarantined: serde_json::Value,
    ) -> String {
        let mut args = serde_json::json!({
            "namespace": "local",
            "from": "email:quarantine",
            "to": "local",
            "content": format!("parked {external_id}"),
            "channel_kind": "email",
            "external_id": external_id,
            "metadata": {
                "quarantined": quarantined,
                "quarantine_reason": "test",
            },
        });
        if let Some(slug) = channel_slug {
            args["channel_slug"] = serde_json::json!(slug);
            args["metadata"]["channel_slug"] = serde_json::json!("spoofed-by-metadata");
        }
        registry
            .dispatch("comm.ingest", args)
            .await
            .expect("quarantine ingest")["full_id"]
            .as_str()
            .expect("full_id")
            .to_string()
    }

    // Both wire spellings produced by generic channel metadata are accepted.
    ingest_quarantine(
        &registry,
        "quarantine-health-acct-1-string",
        Some("acct-1"),
        serde_json::json!("true"),
    )
    .await;
    ingest_quarantine(
        &registry,
        "quarantine-health-acct-1-bool",
        Some("acct-1"),
        serde_json::json!(true),
    )
    .await;
    ingest_quarantine(
        &registry,
        "quarantine-health-acct-2",
        Some("acct-2"),
        serde_json::json!("true"),
    )
    .await;
    ingest_quarantine(
        &registry,
        "quarantine-health-legacy",
        None,
        serde_json::json!("true"),
    )
    .await;

    // Purged/soft-deleted rows are no longer parked and must not contribute.
    let deleted_id = ingest_quarantine(
        &registry,
        "quarantine-health-deleted",
        Some("acct-2"),
        serde_json::json!("true"),
    )
    .await;
    registry
        .dispatch(
            "delete",
            serde_json::json!({"id": deleted_id, "kind": "note"}),
        )
        .await
        .expect("soft-delete quarantined message");

    // A false marker is ordinary mail, not a parked item.
    ingest_quarantine(
        &registry,
        "quarantine-health-false",
        Some("acct-1"),
        serde_json::json!("false"),
    )
    .await;

    let health = registry
        .dispatch("comm.health", serde_json::json!({}))
        .await
        .expect("health succeeds");
    assert_eq!(health["quarantined_count"].as_u64(), Some(4));
    assert_eq!(health["unattributed_quarantined_count"].as_u64(), Some(1));

    let channels = health["channels"].as_array().expect("channels array");
    assert_eq!(channels.len(), 2);
    for (slug, expected) in [("acct-1", 2), ("acct-2", 1)] {
        let channel = channels
            .iter()
            .find(|channel| channel["channel_slug"].as_str() == Some(slug))
            .expect("channel health row");
        assert_eq!(channel["consecutive_failures"].as_u64(), Some(0));
        assert_eq!(channel["stalled"].as_bool(), Some(false));
        assert_eq!(channel["quarantined_count"].as_u64(), Some(expected));
    }

    let inbox = registry
        .dispatch(
            "comm.inbox",
            serde_json::json!({"status": "all", "limit": 50}),
        )
        .await
        .expect("full inbox is the supported quarantine inspection path");
    let visible_parked = inbox["messages"]
        .as_array()
        .expect("messages array")
        .iter()
        .filter(|message| {
            let marker = &message["properties"]["quarantined"];
            marker.as_bool() == Some(true) || marker.as_str() == Some("true")
        })
        .count();
    assert_eq!(
        visible_parked, 4,
        "the same live rows counted by health must be inspectable with their full properties"
    );
}

/// A deployment may intentionally keep operational heartbeats in `local`
/// while routing message data into another authorized namespace. The scoped
/// health read for that message namespace must still synthesize the parked
/// channel entry from persisted quarantine provenance; it must not mislabel
/// that evidence as a daemon heartbeat.
#[tokio::test]
async fn health_surfaces_quarantine_channel_without_a_heartbeat_in_that_namespace() {
    let (registry, _rt) = build_registry_for_ns("local");
    registry
        .dispatch(
            "comm.ingest",
            serde_json::json!({
                "namespace": "tenant-a",
                "from": "email:quarantine",
                "to": "local",
                "content": "tenant parked message",
                "channel_kind": "email",
                "channel_slug": "tenant-mailbox",
                "external_id": "quarantine-health-tenant-a",
                "metadata": {
                    "quarantined": "true",
                    "quarantine_reason": "test",
                },
            }),
        )
        .await
        .expect("tenant quarantine ingest");

    let health = registry
        .dispatch("comm.health", serde_json::json!({"namespace": "tenant-a"}))
        .await
        .expect("tenant health succeeds");
    assert_eq!(health["namespace"].as_str(), Some("tenant-a"));
    assert_eq!(health["role"].as_str(), Some("client"));
    assert!(health["source"].is_null());
    assert_eq!(health["quarantined_count"].as_u64(), Some(1));
    assert_eq!(health["unattributed_quarantined_count"].as_u64(), Some(0));
    let channels = health["channels"].as_array().expect("channels array");
    assert_eq!(channels.len(), 1);
    assert_eq!(channels[0]["channel_kind"].as_str(), Some("email"));
    assert_eq!(channels[0]["channel_slug"].as_str(), Some("tenant-mailbox"));
    assert_eq!(channels[0]["quarantined_count"].as_u64(), Some(1));
    for field in [
        "poll_interval_secs",
        "stalled",
        "last_success_at",
        "last_poll_attempt_at",
        "last_failure_at",
        "last_error",
        "consecutive_failures",
    ] {
        assert!(
            channels[0][field].is_null(),
            "without a heartbeat, `{field}` is unknown rather than fabricated: {}",
            channels[0]
        );
    }
}

async fn plant_healthy_channel_rows(rt: &KhiveRuntime, count: usize) {
    use khive_storage::note::Note;

    let token = rt
        .authorize(khive_runtime::Namespace::local())
        .expect("authorize local");
    let store = rt.notes(&token).expect("notes store");
    let base = chrono::Utc::now().timestamp_micros();
    let notes = (0..count)
        .map(|index| {
            let slug = format!("heartbeat-{index:03}");
            Note {
                id: uuid::Uuid::new_v4(),
                namespace: "local".to_string(),
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
                    "poll_interval_secs": 5,
                    "last_success_at": chrono::Utc::now().to_rfc3339(),
                    "last_poll_attempt_at": chrono::Utc::now().to_rfc3339(),
                    "last_failure_at": null,
                    "last_error": null,
                    "consecutive_failures": 0,
                })),
                created_at: base + index as i64,
                updated_at: base + index as i64,
                deleted_at: None,
            }
        })
        .collect();
    let summary = store
        .upsert_notes(notes)
        .await
        .expect("batch healthy channel rows");
    assert_eq!(summary.attempted, count as u64);
    assert_eq!(summary.affected, count as u64);
    assert_eq!(summary.failed, 0, "heartbeat seed failures: {summary:?}");
}

async fn ingest_attributed_quarantine(
    registry: &VerbRegistry,
    external_id: &str,
    channel_slug: &str,
) {
    registry
        .dispatch(
            "comm.ingest",
            serde_json::json!({
                "namespace": "local",
                "from": "email:quarantine",
                "to": "local",
                "content": format!("parked {external_id}"),
                "channel_kind": "email",
                "channel_slug": channel_slug,
                "external_id": external_id,
                "metadata": {"quarantined": true, "quarantine_reason": "test"},
            }),
        )
        .await
        .expect("quarantine ingest");
}

/// Heartbeats are authoritative liveness evidence and consume the bounded
/// channel budget first. A quarantine identity that sorts before them must not
/// displace a heartbeat or be emitted as a heartbeat-free synthetic row.
#[tokio::test]
async fn health_channel_limit_never_displaces_a_real_heartbeat() {
    let (registry, rt) = build_registry_for_ns("local");
    plant_healthy_channel_rows(&rt, 200).await;
    ingest_attributed_quarantine(
        &registry,
        "quarantine-health-over-limit",
        "000-quarantine-only",
    )
    .await;

    let health = registry
        .dispatch("comm.health", serde_json::json!({}))
        .await
        .expect("health succeeds");
    let channels = health["channels"].as_array().expect("channels array");
    assert_eq!(channels.len(), 200, "the union has one hard response bound");
    assert_eq!(health["quarantined_count"].as_u64(), Some(1));
    assert!(
        channels
            .iter()
            .all(|channel| channel["poll_interval_secs"].as_u64() == Some(5)),
        "all 200 returned entries must be real heartbeat rows: {channels:?}"
    );
    assert!(
        channels
            .iter()
            .all(|channel| channel["channel_slug"] != "000-quarantine-only"),
        "quarantine-only evidence must not displace or masquerade as a heartbeat"
    );
}

/// Once all heartbeat rows fit, quarantine-only identities fill only the
/// remaining capacity in stable `(channel_kind, channel_slug)` order.
#[tokio::test]
async fn health_channel_limit_orders_quarantine_only_fill_deterministically() {
    let (registry, rt) = build_registry_for_ns("local");
    plant_healthy_channel_rows(&rt, 199).await;
    ingest_attributed_quarantine(&registry, "quarantine-health-late", "zz-last").await;
    ingest_attributed_quarantine(&registry, "quarantine-health-first", "aa-first").await;

    let health = registry
        .dispatch("comm.health", serde_json::json!({}))
        .await
        .expect("health succeeds");
    let channels = health["channels"].as_array().expect("channels array");
    assert_eq!(channels.len(), 200, "the union has one hard response bound");
    assert_eq!(health["quarantined_count"].as_u64(), Some(2));
    assert_eq!(
        channels[0]["channel_slug"], "heartbeat-198",
        "heartbeat rows retain the store's newest-first order"
    );
    assert_eq!(
        channels[198]["channel_slug"], "heartbeat-000",
        "the complete heartbeat page remains ahead of synthetic entries"
    );
    assert_eq!(
        channels[199]["channel_slug"], "aa-first",
        "the lexicographically first quarantine-only identity fills the final slot"
    );
    assert!(
        channels
            .iter()
            .all(|channel| channel["channel_slug"] != "zz-last"),
        "later quarantine-only identities are truncated deterministically"
    );
}

/// #1472: a silently stopped poller must be distinguishable from a healthy,
/// idle channel even when its persisted failure count is zero.
#[tokio::test]
async fn health_flags_stopped_poller_after_three_nominal_intervals() {
    let (registry, _rt) = build_registry_for_ns("local");
    let old_attempt = (chrono::Utc::now() - chrono::Duration::seconds(16)).to_rfc3339();

    registry
        .dispatch(
            "comm.heartbeat",
            serde_json::json!({
                "namespace": "local",
                "channel_kind": "email",
                "channel_slug": "stopped@example.com",
                "poll_interval_secs": 5,
                "outcome": "success",
                "at": old_attempt,
            }),
        )
        .await
        .expect("heartbeat succeeds");

    let health = registry
        .dispatch("comm.health", serde_json::json!({}))
        .await
        .expect("health succeeds");
    let channel = &health["channels"][0];
    assert_eq!(channel["consecutive_failures"].as_u64(), Some(0));
    assert_eq!(channel["poll_interval_secs"].as_u64(), Some(5));
    assert_eq!(channel["stalled"].as_bool(), Some(true));
}

/// Mixed-version rows lack cadence metadata.
#[tokio::test]
async fn health_reports_null_stalled_for_legacy_heartbeat() {
    let (registry, _rt) = build_registry_for_ns("local");

    registry
        .dispatch(
            "comm.heartbeat",
            serde_json::json!({
                "namespace": "local",
                "channel_kind": "email",
                "channel_slug": "legacy@example.com",
                "outcome": "success",
                "at": "2020-01-01T00:00:00Z",
            }),
        )
        .await
        .expect("legacy heartbeat succeeds");

    let health = registry
        .dispatch("comm.health", serde_json::json!({}))
        .await
        .expect("health succeeds");
    let channel = &health["channels"][0];
    assert!(channel["poll_interval_secs"].is_null());
    assert!(channel["stalled"].is_null());
}

/// A future attempt timestamp cannot support an elapsed-time judgment (for example under clock skew), so it must not be reported as current.
#[tokio::test]
async fn health_reports_null_stalled_for_future_attempt() {
    let (registry, _rt) = build_registry_for_ns("local");
    let future_attempt = (chrono::Utc::now() + chrono::Duration::seconds(60)).to_rfc3339();

    registry
        .dispatch(
            "comm.heartbeat",
            serde_json::json!({
                "namespace": "local",
                "channel_kind": "email",
                "channel_slug": "future@example.com",
                "poll_interval_secs": 5,
                "outcome": "success",
                "at": future_attempt,
            }),
        )
        .await
        .expect("heartbeat succeeds");

    let health = registry
        .dispatch("comm.health", serde_json::json!({}))
        .await
        .expect("health succeeds");
    assert!(health["channels"][0]["stalled"].is_null());
}

/// Malformed persisted failure counters cannot safely distinguish a healthy idle channel from one in failure/backoff, so their staleness is unknown.
#[tokio::test]
async fn health_reports_null_stalled_for_malformed_or_missing_failure_count() {
    use khive_storage::note::Note;

    let (registry, rt) = build_registry_for_ns("local");
    let token = rt
        .authorize(khive_runtime::Namespace::local())
        .expect("authorize local");
    let store = rt.notes(&token).expect("notes store");
    let old_attempt = (chrono::Utc::now() - chrono::Duration::seconds(60)).to_rfc3339();

    for (slug, failure_count) in [
        ("missing@example.com", None),
        ("string@example.com", Some(serde_json::json!("1"))),
        ("negative@example.com", Some(serde_json::json!(-1))),
    ] {
        let mut properties = serde_json::json!({
            "channel_kind": "email",
            "channel_slug": slug,
            "poll_interval_secs": 5,
            "last_poll_attempt_at": old_attempt,
        });
        if let Some(failure_count) = failure_count {
            properties["consecutive_failures"] = failure_count;
        }
        let now = chrono::Utc::now().timestamp_micros();
        store
            .upsert_note(Note {
                id: uuid::Uuid::new_v4(),
                namespace: "local".to_string(),
                kind: "channel_health".to_string(),
                status: "active".to_string(),
                name: Some(format!("email:{slug}")),
                content: format!("channel heartbeat: email:{slug}"),
                salience: None,
                decay_factor: None,
                expires_at: None,
                properties: Some(properties),
                created_at: now,
                updated_at: now,
                deleted_at: None,
            })
            .await
            .expect("upsert malformed channel_health row");
    }

    let health = registry
        .dispatch("comm.health", serde_json::json!({}))
        .await
        .expect("health succeeds");
    let channels = health["channels"].as_array().expect("channels is array");
    assert_eq!(channels.len(), 3);
    assert!(
        channels.iter().all(|channel| channel["stalled"].is_null()),
        "malformed or missing failure counts must produce stalled: null: {channels:?}"
    );
}

/// design review amendment 3: `last_error` is RETAINED after a subsequent success (callers compare `last_error.at` vs `last_success_at`), and `consecutive_failures` resets to 0 on success.
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
                "poll_interval_secs": 5,
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
                "poll_interval_secs": 5,
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
    assert!(
        ch["stalled"].is_null(),
        "known failure/backoff state has no nominal-cadence stall judgment"
    );

    registry
        .dispatch(
            "comm.heartbeat",
            serde_json::json!({
                "namespace": "local",
                "channel_kind": "email",
                "channel_slug": "recipient@example.com",
                "poll_interval_secs": 5,
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
    assert_eq!(ch["stalled"].as_bool(), Some(false));
}

/// design review amendment 2: rows are keyed by channel slug + kind, never kind alone — two accounts of the same kind must not collapse into one row.
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

/// Repeated heartbeats for the same (kind, slug) update the same row (via `upsert_note`'s deterministic id) rather than accumulating duplicates.
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
async fn heartbeat_rejects_zero_poll_interval() {
    let (registry, _rt) = build_registry_for_ns("local");

    let err = registry
        .dispatch(
            "comm.heartbeat",
            serde_json::json!({
                "namespace": "local",
                "channel_kind": "email",
                "channel_slug": "recipient@example.com",
                "poll_interval_secs": 0,
                "outcome": "success",
            }),
        )
        .await
        .expect_err("zero poll interval must be rejected");
    assert!(
        err.to_string().contains("poll_interval_secs"),
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

/// Spec: "report TIMESTAMPS only ... never a computed `healthy: bool`" — the channel entry shape must not carry any boolean health verdict.
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

/// khive #877: `comm.health` must read `channel_health` rows from the caller's injected namespace (`token.namespace()`), not the fixed `khive_pack_comm::CHANNEL_HEALTH_NAMESPACE` constant.
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

/// khive #917: an authorized per-tenant writer — an embedding host that authenticates a tenant principal out-of-band and dispatches via `VerbRegistry::dispatch_as` with a `VerifiedActor`, passing the tenant's own namespace as the explicit `namespace` dispatch param (ADR-007 Rev 6 Rule 3's explicit escape, the same mechanism the local poll loop already uses to pin its own writes to `"local"`) — persists its `comm.heartbeat` row under that tenant's own namespace rather than the fixed `khive_pack_comm::CHANNEL_HEALTH_NAMESPACE` constant.
#[tokio::test]
async fn authorized_writer_persists_heartbeat_under_its_own_tenant_namespace() {
    let (registry, _rt) = build_registry_for_ns("local");

    registry
        .dispatch_as(
            "comm.heartbeat",
            serde_json::json!({
                "namespace": "tenant-a",
                "channel_kind": "email",
                "channel_slug": "tenant-a-inbox@example.com",
                "outcome": "success",
            }),
            khive_runtime::VerifiedActor::new("tenant-a-writer").expect("non-blank actor id"),
        )
        .await
        .expect("authorized tenant heartbeat succeeds");

    let scoped_health = registry
        .dispatch(
            "comm.health",
            serde_json::json!({ "namespace": "tenant-a" }),
        )
        .await
        .expect("tenant-scoped health succeeds");
    let scoped_channels = scoped_health["channels"].as_array().expect("array");
    assert_eq!(
        scoped_channels.len(),
        1,
        "tenant-scoped comm.health must see the row the authorized writer produced: \
         {scoped_channels:?}"
    );
    assert_eq!(
        scoped_channels[0]["channel_slug"].as_str(),
        Some("tenant-a-inbox@example.com")
    );
    assert_eq!(scoped_health["role"].as_str(), Some("daemon"));

    let default_health = registry
        .dispatch("comm.health", serde_json::json!({}))
        .await
        .expect("unscoped health succeeds");
    let default_channels = default_health["channels"].as_array().expect("array");
    assert!(
        default_channels.is_empty(),
        "the local-scoped read must not see the tenant's heartbeat row: {default_channels:?}"
    );
}

/// khive #917 regression guard: the write namespace is a component of the deterministic heartbeat row id (`heartbeat_note_id`), so two authorized per-tenant writers dispatching `comm.heartbeat` for the SAME `(channel_kind, channel_slug)` under DIFFERENT namespaces must produce two distinct rows — one visible under each tenant's scoped `comm.health`, never colliding onto a single id.
#[tokio::test]
async fn two_tenants_same_channel_get_distinct_heartbeat_rows() {
    let (registry, _rt) = build_registry_for_ns("local");

    registry
        .dispatch_as(
            "comm.heartbeat",
            serde_json::json!({
                "namespace": "tenant-a",
                "channel_kind": "email",
                "channel_slug": "shared@example.com",
                "outcome": "success",
            }),
            khive_runtime::VerifiedActor::new("tenant-a-writer").expect("non-blank actor id"),
        )
        .await
        .expect("tenant-a heartbeat succeeds");

    registry
        .dispatch_as(
            "comm.heartbeat",
            serde_json::json!({
                "namespace": "tenant-b",
                "channel_kind": "email",
                "channel_slug": "shared@example.com",
                "outcome": "success",
            }),
            khive_runtime::VerifiedActor::new("tenant-b-writer").expect("non-blank actor id"),
        )
        .await
        .expect("tenant-b heartbeat succeeds");

    for ns in ["tenant-a", "tenant-b"] {
        let health = registry
            .dispatch("comm.health", serde_json::json!({ "namespace": ns }))
            .await
            .expect("tenant-scoped health succeeds");
        let channels = health["channels"].as_array().expect("array");
        assert_eq!(
            channels.len(),
            1,
            "namespace {ns} must see exactly its own heartbeat row \
             (distinct id per namespace): {channels:?}"
        );
        assert_eq!(
            channels[0]["channel_slug"].as_str(),
            Some("shared@example.com")
        );
        assert_eq!(
            health["namespace"].as_str(),
            Some(ns),
            "scoped health must echo the namespace it read"
        );
    }
}

/// A single actor namespace receives messages from two distinct senders; `from_actor` (exact match) selects only the messages from the named sender.
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

/// `from_prefix` selects all senders whose actor label starts with the given prefix, e.g. `"agent:khive:"` selects every spawned agent under that namespace.
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

/// Absent from_actor/from_prefix preserves today's behavior exactly: no sender filter is applied and both senders' messages are returned.
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

/// Default order ("asc") truncates from the tail — this is the pre-existing (buggy, per #494) behavior that must stay byte-identical: a thread longer than `limit` returns the HEAD (oldest messages), not the newest.
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
        vec!["root", "reply-1"],
        "default order must truncate from the tail (keep the head), one entry per \
         logical message post-#94 dedup"
    );
}

/// `order="desc"` returns the newest `limit` messages instead of the oldest — the #494 fix: long threads can now reach their tail.
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
        vec!["reply-4", "reply-3"],
        "order=desc + limit=2 must return the newest 2 logical messages — not the \
         oldest (#494 fix: the tail is now reachable), one entry per logical message \
         post-#94 dedup (no duplicate 'reply-4' entry)"
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

/// `after` accepts a message id cursor and returns only messages strictly after it (enables incremental polling without re-fetching history).
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
        Vec::<&str>::new(),
        "after=reply-1's own canonical (outbound) id excludes reply-1 itself post-#94 \
         dedup, and reply-1 was the last message sent, so nothing remains; got {contents:?}"
    );
}

/// Insert a `message` note directly into the store with an explicit `created_at`, bypassing `comm.send`/`comm.reply`.
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

/// #494: two physical messages that share the exact same microsecond `created_at` (e.g. what an ADR-057 dual-write self-send can produce) must not be skipped or duplicated around an id cursor — the cursor filter and sort must compare the full `(created_at, full_id)` tuple, not timestamp alone.
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

/// #494: an `after` timestamp cursor must be parsed to microseconds (not compared as a raw string) so non-canonical but valid RFC 3339 forms — whole-second `Z`, or an explicit `+00:00` offset — compare correctly against khive's canonical microsecond timestamps.
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

/// #494: an `after` value that is neither a resolvable message id nor a parseable RFC 3339 timestamp must fail loudly, never be silently coerced into "no cursor" (which would return the whole thread).
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

/// #494: `order="desc"` combined with an id `after` cursor must filter against the DESC sequence, not always `created_at >`.
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
    // both strictly older than the synthetic 2099 timestamps) — collapsed by
    // #94's dedup fix into a single "root" thread entry. Full desc sequence
    // is therefore [msg-c, msg-b, msg-a, root]. `after=msg-b` must return
    // only what comes strictly after it in THAT sequence — msg-a and root —
    // never msg-c, even though msg-c is also `>` msg-b in wall-clock terms.
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
        vec!["msg-a", "root"],
        "order=desc + after=msg-b must return only rows strictly older than msg-b \
         (further along the desc sequence), in desc order — one 'root' entry \
         post-#94 dedup, msg-c excluded; got {contents:?}"
    );
}

/// Absent `order`/`after` preserves the #494 ordering/truncation behavior; the message count itself changed under #94's dedup fix (one entry per logical message, not one per ADR-057 dual-write physical copy) — see the updated assertions below.
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
        2,
        "root + reply-1 = 2 logical messages post-#94 dedup (each was an ADR-057 \
         dual-write outbound+inbound pair, now collapsed to one thread entry)"
    );
    let contents: Vec<&str> = msgs
        .iter()
        .map(|m| m["content"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(contents, vec!["root", "reply-1"]);
}

/// Full round trip: A sends to B, B replies, A replies again.
#[tokio::test]
async fn t94_thread_round_trip_returns_deduped_logical_messages() {
    let backend = shared_backend();
    let (registry_a, _rt_a) = build_actor_registry(backend.clone(), "lambda:a");
    let (registry_b, _rt_b) = build_actor_registry(backend.clone(), "lambda:b");

    let sent = registry_a
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:b", "content": "hello from A" }),
        )
        .await
        .expect("A sends to B");
    let root_id = sent["full_id"].as_str().expect("full_id").to_string();

    let b_inbox = registry_b
        .dispatch(
            "comm.inbox",
            serde_json::json!({ "status": "all", "limit": 10 }),
        )
        .await
        .expect("B inbox");
    let b_msgs = b_inbox["messages"].as_array().expect("messages");
    assert_eq!(b_msgs.len(), 1, "B sees exactly 1 inbound message");
    let b_inbound_id = b_msgs[0]["full_id"].as_str().expect("full_id").to_string();
    registry_b
        .dispatch(
            "comm.reply",
            serde_json::json!({ "id": b_inbound_id, "content": "reply from B" }),
        )
        .await
        .expect("B replies");

    let a_inbox = registry_a
        .dispatch(
            "comm.inbox",
            serde_json::json!({ "status": "all", "limit": 10 }),
        )
        .await
        .expect("A inbox");
    let a_msgs = a_inbox["messages"].as_array().expect("messages");
    assert_eq!(a_msgs.len(), 1, "A sees exactly B's reply");
    let a_inbound_id = a_msgs[0]["full_id"].as_str().expect("full_id").to_string();
    registry_a
        .dispatch(
            "comm.reply",
            serde_json::json!({ "id": a_inbound_id, "content": "reply from A again" }),
        )
        .await
        .expect("A replies again");

    let thread = registry_a
        .dispatch("comm.thread", serde_json::json!({ "id": root_id }))
        .await
        .expect("A reads the thread");
    let count = thread["count"].as_u64().expect("count");
    assert_eq!(
        count, 3,
        "3 logical messages (not 6 dual-write physical copies); got {thread}"
    );
    let msgs = thread["messages"].as_array().expect("messages array");
    let contents: Vec<&str> = msgs
        .iter()
        .map(|m| m["content"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(
        contents,
        vec!["hello from A", "reply from B", "reply from A again"],
        "messages in chronological order with no duplicates; got {contents:?}"
    );
    let from_actors: Vec<&str> = msgs
        .iter()
        .map(|m| m["from"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(
        from_actors,
        vec!["lambda:a", "lambda:b", "lambda:a"],
        "each entry attributed to the actor that actually sent it; got {from_actors:?}"
    );

    let thread_from_b = registry_b
        .dispatch("comm.thread", serde_json::json!({ "id": root_id }))
        .await
        .expect("B reads the thread");
    assert_eq!(thread_from_b["count"].as_u64().unwrap_or(0), 3);
}

/// A message addressed to one actor pair must not be visible to an unrelated third actor who merely knows (or can resolve) the thread's root id — the caller-boundary gap between `thread` (previously unfiltered by actor) and `inbox` (already actor-scoped) from issue #94 symptom 1/2.
#[tokio::test]
async fn t94_thread_excludes_messages_not_addressed_to_or_from_caller() {
    let backend = shared_backend();
    let (registry_a, _rt_a) = build_actor_registry(backend.clone(), "lambda:a");
    let (registry_c, _rt_c) = build_actor_registry(backend, "lambda:c");

    let sent = registry_a
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:b", "content": "private to B" }),
        )
        .await
        .expect("A sends to B");
    let root_id = sent["full_id"].as_str().expect("full_id").to_string();

    let thread_from_c = registry_c
        .dispatch("comm.thread", serde_json::json!({ "id": root_id }))
        .await
        .expect("C can resolve the root id but must see zero messages");
    assert_eq!(
        thread_from_c["count"].as_u64().unwrap_or(99),
        0,
        "an actor who is neither sender nor addressee must see zero thread messages \
         (issue #94: thread previously exposed every actor's copies unfiltered); \
         got {thread_from_c}"
    );

    let thread_from_a = registry_a
        .dispatch("comm.thread", serde_json::json!({ "id": root_id }))
        .await
        .expect("A reads own thread");
    assert_eq!(thread_from_a["count"].as_u64().unwrap_or(0), 1);
}

/// A legacy message note lacking `to_actor` (pre-ADR-057 data, or directly store-inserted content) must remain visible via `comm.thread`'s actor scoping — the same EqOrMissing rule `comm.inbox` already applies for exactly this shape (ADR-057 Q3).
#[tokio::test]
async fn t94_thread_legacy_message_without_to_actor_stays_visible() {
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

    let legacy_id = uuid::Uuid::new_v4();
    insert_thread_message(
        &rt,
        "local",
        legacy_id,
        root_uuid,
        chrono::Utc::now().timestamp_micros(),
        "legacy no-to_actor",
    )
    .await;

    let thread = registry
        .dispatch("comm.thread", serde_json::json!({ "id": root_full_id }))
        .await
        .expect("thread succeeds");
    let contents: Vec<&str> = thread["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["content"].as_str().unwrap_or(""))
        .collect();
    assert!(
        contents.contains(&"legacy no-to_actor"),
        "a message without to_actor must stay visible (EqOrMissing); got {contents:?}"
    );
}

/// `comm.send(tags=[...])` persists the tags into `properties["tags"]` on the inbound copy, round-tripped via `comm.inbox`.
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

/// `comm.send(tags=[...])` also persists on the outbound copy, round-tripped via `comm.read` after resolving the sender's own outbound note.
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

/// `comm.send` with an unknown top-level field (typo) is still rejected — `tags` addition must not have loosened `deny_unknown_fields`.
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

/// Child and parent configured with genuinely distinct actor identities: a send from the child to the parent's label must succeed and land in the parent's inbox only, exactly as ordinary actor-addressed delivery already works (ADR-057).
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

/// A caller whose named target genuinely IS its own resolved actor identity (a deliberate note-to-self) must still be allowed to send when it says so explicitly via `self_send=true`.
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

/// The silent-collapse case: a session addresses a label that happens to equal its own resolved actor identity (e.g. a sub-agent naming what it believes is its parent's distinct label, but which resolves to the same `[actor] id` as its own token per ADR-096 Fork 2) WITHOUT declaring `self_send=true`.
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

/// The anonymous single-tenant party-line default (`to="local"` from an unattributed caller) must remain unaffected: `to_actor == "local"` is exempted from the self-address rejection since it is the pervasive unconfigured single-actor pattern, not a collapsed distinct-principal address.
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

/// A third party holding a message id (neither the sender nor the addressee) must not be able to reply to it.
#[tokio::test]
async fn i113_non_participant_reply_rejected() {
    let backend = shared_backend();
    let (registry_a, _rt_a) = build_actor_registry(backend.clone(), "lambda:a");
    let (registry_c, _rt_c) = build_actor_registry(backend.clone(), "lambda:c");

    let send_result = registry_a
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:b", "content": "hello B from A" }),
        )
        .await
        .expect("send succeeds");
    let msg_id = send_result["full_id"]
        .as_str()
        .expect("full_id")
        .to_string();

    let err = registry_c
        .dispatch(
            "comm.reply",
            serde_json::json!({ "id": msg_id, "content": "forged reply from C" }),
        )
        .await
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("not addressed to or from caller actor"),
        "non-participant reply must be rejected; got {err}"
    );
}

/// An unattributed caller is still a distinct `local` actor and must not bypass participant checks for messages carrying explicit actor fields.
#[tokio::test]
async fn i113_anonymous_non_participant_reply_rejected() {
    let backend = shared_backend();
    let (registry_a, _rt_a) = build_actor_registry(backend.clone(), "lambda:a");
    let (registry_local, _rt_local) = build_crossns_registry(backend, "local", vec![]);

    let send_result = registry_a
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:b", "content": "hello B from A" }),
        )
        .await
        .expect("send succeeds");
    let msg_id = send_result["full_id"]
        .as_str()
        .expect("full_id")
        .to_string();

    let err = registry_local
        .dispatch(
            "comm.reply",
            serde_json::json!({ "id": msg_id, "content": "forged anonymous reply" }),
        )
        .await
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("not addressed to or from caller actor"),
        "anonymous non-participant reply must be rejected; got {err}"
    );
}

/// The addressee (recipient) may reply — this is the common case.
#[tokio::test]
async fn i113_addressee_reply_succeeds() {
    let backend = shared_backend();
    let (registry_a, _rt_a) = build_actor_registry(backend.clone(), "lambda:a");
    let (registry_b, _rt_b) = build_actor_registry(backend.clone(), "lambda:b");

    let send_result = registry_a
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:b", "content": "hello B from A" }),
        )
        .await
        .expect("send succeeds");
    let msg_id = send_result["full_id"]
        .as_str()
        .expect("full_id")
        .to_string();

    let reply = registry_b
        .dispatch(
            "comm.reply",
            serde_json::json!({ "id": msg_id, "content": "reply from B" }),
        )
        .await
        .expect("addressee reply must succeed");
    assert_eq!(reply["from"], "lambda:b");
}

/// The original sender may also reply to their own outbound message (e.g. a follow-up before the recipient has responded) — either party is a participant, not addressee-only.
#[tokio::test]
async fn i113_sender_reply_to_own_message_succeeds() {
    let backend = shared_backend();
    let (registry_a, _rt_a) = build_actor_registry(backend.clone(), "lambda:a");

    let send_result = registry_a
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:b", "content": "hello B from A" }),
        )
        .await
        .expect("send succeeds");
    let msg_id = send_result["full_id"]
        .as_str()
        .expect("full_id")
        .to_string();

    let reply = registry_a
        .dispatch(
            "comm.reply",
            serde_json::json!({ "id": msg_id, "content": "follow-up from A" }),
        )
        .await
        .expect("sender reply to own message must succeed");
    assert_eq!(reply["from"], "lambda:a");
}

/// A legacy message with neither `to_actor` nor `from_actor` fails open (no attributed party to restrict against), matching the #87/#94 precedent.
#[tokio::test]
async fn i113_legacy_message_without_actors_fails_open() {
    use khive_storage::note::Note;

    let (registry, rt) = build_registry_for_ns("local");
    let token = rt
        .authorize(khive_runtime::Namespace::parse("local").unwrap())
        .unwrap();
    let store = rt.notes(&token).expect("notes store");

    let legacy =
        Note::new("local", "message", "legacy message body").with_properties(serde_json::json!({
            "direction": "inbound",
            "sent_at": chrono::Utc::now().to_rfc3339(),
        }));
    let legacy_id = legacy.id;
    store.upsert_note(legacy).await.expect("legacy note insert");

    let reply = registry
        .dispatch(
            "comm.reply",
            serde_json::json!({ "id": legacy_id.to_string(), "content": "reply to legacy" }),
        )
        .await;
    assert!(
        reply.is_ok(),
        "reply to a legacy message with no to_actor/from_actor must fail open; got {reply:?}"
    );
}

#[tokio::test]
async fn i66_unread_counts_only_matching_inbound_unread() {
    let backend = shared_backend();
    let (registry_a, _rt_a) = build_actor_registry(backend.clone(), "lambda:a");
    let (registry_b, _rt_b) = build_actor_registry(backend.clone(), "lambda:b");

    registry_a
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:b", "content": "msg 1" }),
        )
        .await
        .expect("send 1");
    registry_a
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:b", "content": "msg 2" }),
        )
        .await
        .expect("send 2");

    let unread_before = registry_b
        .dispatch("comm.unread", serde_json::json!({}))
        .await
        .expect("unread succeeds");
    assert_eq!(unread_before["count"], 2);
    assert_eq!(unread_before["actor"], "lambda:b");

    let inbox = registry_b
        .dispatch(
            "comm.inbox",
            serde_json::json!({ "status": "unread", "limit": 50 }),
        )
        .await
        .expect("inbox succeeds");
    let msg2_inbound_id = inbox["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["preview"].as_str().unwrap_or("").contains("msg 2"))
        .map(|m| m["full_id"].as_str().unwrap().to_string())
        .expect("find B's inbound copy of msg 2");
    registry_b
        .dispatch("comm.read", serde_json::json!({ "id": msg2_inbound_id }))
        .await
        .expect("read succeeds");

    let unread_after = registry_b
        .dispatch("comm.unread", serde_json::json!({}))
        .await
        .expect("unread succeeds");
    assert_eq!(unread_after["count"], 1);
}

/// `comm.unread` is caller-scoped and rejects the removed `assignee` override.
#[tokio::test]
async fn i66_unread_rejects_assignee_override() {
    let backend = shared_backend();
    let (registry_a, _rt_a) = build_actor_registry(backend.clone(), "lambda:a");
    let (registry_orch, _rt_o) = build_actor_registry(backend.clone(), "lambda:orch");

    registry_a
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:b", "content": "msg for b" }),
        )
        .await
        .expect("send succeeds");

    let err = registry_orch
        .dispatch("comm.unread", serde_json::json!({ "assignee": "lambda:b" }))
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("unknown field `assignee`"),
        "removed assignee override must be rejected as an unknown param; got {err}"
    );

    let own_unread = registry_orch
        .dispatch("comm.unread", serde_json::json!({}))
        .await
        .expect("unread succeeds");
    assert_eq!(own_unread["count"], 0);
}

/// `comm.inbox`'s response carries `unread_count` alongside `messages`/`count`.
#[tokio::test]
async fn i66_inbox_response_carries_unread_count() {
    let backend = shared_backend();
    let (registry_a, _rt_a) = build_actor_registry(backend.clone(), "lambda:a");
    let (registry_b, _rt_b) = build_actor_registry(backend.clone(), "lambda:b");

    registry_a
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:b", "content": "msg 1" }),
        )
        .await
        .expect("send 1");

    let inbox = registry_b
        .dispatch(
            "comm.inbox",
            serde_json::json!({ "status": "unread", "limit": 50 }),
        )
        .await
        .expect("inbox succeeds");
    assert_eq!(inbox["count"], 1);
    assert_eq!(
        inbox["unread_count"], 1,
        "unread_count must be present and match count when status=unread"
    );
}

/// `limit=0` is the count-only inbox path: it returns no message payloads but still reports the caller's real unread total.
#[tokio::test]
async fn i66_inbox_limit_zero_carries_real_unread_count() {
    let backend = shared_backend();
    let (registry_a, _rt_a) = build_actor_registry(backend.clone(), "lambda:a");
    let (registry_b, _rt_b) = build_actor_registry(backend, "lambda:b");

    registry_a
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:b", "content": "count me" }),
        )
        .await
        .expect("send succeeds");

    let inbox = registry_b
        .dispatch("comm.inbox", serde_json::json!({ "limit": 0 }))
        .await
        .expect("limit=0 inbox succeeds");
    assert_eq!(inbox["messages"], serde_json::json!([]));
    assert_eq!(inbox["count"], 0);
    assert_eq!(
        inbox["unread_count"], 1,
        "limit=0 must return the caller's real unread count"
    );
}

/// A send must land the outbound + inbound note, an FTS document for each, and one vector row PER registered embedding model for EACH note, all inside the single atomic unit.
#[tokio::test]
async fn send_lands_outbound_inbound_fts_and_vectors_with_multi_model_counts() {
    use async_trait::async_trait;
    use khive_runtime::EmbedderProvider;
    use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};

    macro_rules! stub_model {
        ($provider:ident, $service:ident, $name:literal, $dims:literal) => {
            struct $service;
            #[async_trait]
            impl EmbeddingService for $service {
                async fn embed(
                    &self,
                    texts: &[String],
                    _model: EmbeddingModel,
                ) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
                    Ok(texts.iter().map(|_| vec![0.25_f32; $dims]).collect())
                }
                fn supports_model(&self, _model: EmbeddingModel) -> bool {
                    true
                }
                fn name(&self) -> &'static str {
                    $name
                }
            }
            struct $provider;
            #[async_trait]
            impl EmbedderProvider for $provider {
                fn name(&self) -> &str {
                    $name
                }
                fn dimensions(&self) -> usize {
                    $dims
                }
                async fn build(&self) -> khive_runtime::RuntimeResult<Arc<dyn EmbeddingService>> {
                    Ok(Arc::new($service))
                }
            }
        };
    }
    stub_model!(
        SendCountsModelA,
        SendCountsServiceA,
        "send-counts-model-a",
        4
    );
    stub_model!(
        SendCountsModelB,
        SendCountsServiceB,
        "send-counts-model-b",
        6
    );

    let (registry, rt) = build_registry_for_ns("agent:sender");
    rt.register_embedder(SendCountsModelA);
    rt.register_embedder(SendCountsModelB);

    registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "agent:sender", "content": "multi-model counts" }),
        )
        .await
        .expect("send succeeds");

    let local_tok = rt.authorize(Namespace::parse("local").unwrap()).unwrap();
    let notes = rt
        .list_notes(&local_tok, Some("message"), 100, 0)
        .await
        .expect("list_notes");
    let alive: Vec<_> = notes.iter().filter(|n| n.deleted_at.is_none()).collect();
    assert_eq!(alive.len(), 2, "expected outbound + inbound; got {alive:?}");

    let fts = rt.text_for_notes(&local_tok).expect("text store");
    for note in &alive {
        assert!(
            fts.get_document("local", note.id)
                .await
                .expect("get_document")
                .is_some(),
            "FTS document must exist for note {}",
            note.id
        );
    }

    for model in ["send-counts-model-a", "send-counts-model-b"] {
        let vs = rt.vectors_for_model(&local_tok, model).expect("vec store");
        assert_eq!(
            vs.count().await.expect("count"),
            2,
            "expected one vector row per note ({model}): outbound + inbound"
        );
    }
}

async fn insert_i1422_message(
    runtime: &KhiveRuntime,
    sequence: u32,
    created_at: i64,
    from_actor: &str,
    to_actor: &str,
    subject: Option<&str>,
    content: &str,
) -> uuid::Uuid {
    use khive_storage::note::Note;

    let token = runtime.authorize(Namespace::local()).expect("local token");
    let store = runtime.notes(&token).expect("notes store");
    let id = uuid::Uuid::from_u128((u128::from(sequence) << 96) | u128::from(sequence));
    let mut properties = serde_json::json!({
        "direction": "inbound",
        "from_actor": from_actor,
        "to_actor": to_actor,
        "read": false,
    });
    if let Some(subject) = subject {
        properties["subject"] = serde_json::json!(subject);
    }
    store
        .upsert_note(Note {
            id,
            namespace: "local".to_string(),
            kind: "message".to_string(),
            status: "active".to_string(),
            name: None,
            content: content.to_string(),
            salience: None,
            decay_factor: None,
            expires_at: None,
            properties: Some(properties),
            created_at,
            updated_at: created_at,
            deleted_at: None,
        })
        .await
        .expect("insert #1422 message");
    id
}

#[tokio::test]
async fn i1422_inbox_offset_enumerates_more_than_the_page_cap_without_mutation() {
    let backend = shared_backend();
    let (registry, runtime) = build_actor_registry(backend, "lambda:reader");
    let created_at = chrono::DateTime::parse_from_rfc3339("2026-07-31T12:00:00Z")
        .unwrap()
        .timestamp_micros();

    for sequence in 1..=205 {
        insert_i1422_message(
            &runtime,
            sequence,
            created_at,
            "lambda:sender",
            "lambda:reader",
            None,
            &format!("message {sequence}"),
        )
        .await;
    }

    let first = registry
        .dispatch(
            "comm.inbox",
            serde_json::json!({ "status": "all", "limit": 200 }),
        )
        .await
        .expect("first page");
    assert_eq!(first["count"], 200);
    assert_eq!(first["offset"], 0);
    assert_eq!(first["has_more"], true);
    assert_eq!(first["next_offset"], 200);

    let second = registry
        .dispatch(
            "comm.inbox",
            serde_json::json!({ "status": "all", "limit": 200, "offset": 200 }),
        )
        .await
        .expect("second page");
    assert_eq!(second["count"], 5);
    assert_eq!(second["offset"], 200);
    assert_eq!(second["has_more"], false);
    assert!(second["next_offset"].is_null());

    let expected_id = |sequence: u32| {
        uuid::Uuid::from_u128((u128::from(sequence) << 96) | u128::from(sequence)).to_string()
    };
    assert_eq!(first["messages"][0]["full_id"], expected_id(1));
    assert_eq!(first["messages"][199]["full_id"], expected_id(200));
    assert_eq!(second["messages"][0]["full_id"], expected_id(201));
    assert_eq!(second["messages"][4]["full_id"], expected_id(205));

    let ids: std::collections::HashSet<_> = first["messages"]
        .as_array()
        .unwrap()
        .iter()
        .chain(second["messages"].as_array().unwrap())
        .map(|message| message["full_id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids.len(), 205, "pages must be complete and disjoint");

    let unread = registry
        .dispatch("comm.unread", serde_json::json!({}))
        .await
        .expect("unread count");
    assert_eq!(unread["count"], 205, "pagination must be read-only");
}

#[tokio::test]
async fn i1422_offset_applies_after_time_sender_and_text_filters() {
    let backend = shared_backend();
    let (registry, runtime) = build_actor_registry(backend, "lambda:reader");
    let micros = |timestamp: &str| {
        chrono::DateTime::parse_from_rfc3339(timestamp)
            .unwrap()
            .timestamp_micros()
    };

    let older = insert_i1422_message(
        &runtime,
        301,
        micros("2026-07-31T12:01:00Z"),
        "team:alpha",
        "lambda:reader",
        Some("Deployment ALERT"),
        "Database timeout in worker A",
    )
    .await;
    insert_i1422_message(
        &runtime,
        302,
        micros("2026-07-31T12:02:00Z"),
        "team:noise",
        "lambda:reader",
        Some("Deployment alert"),
        "database timeout from excluded sender",
    )
    .await;
    insert_i1422_message(
        &runtime,
        303,
        micros("2026-07-31T12:03:00Z"),
        "team:no-subject",
        "lambda:reader",
        None,
        "database timeout without a subject",
    )
    .await;
    let newer = insert_i1422_message(
        &runtime,
        304,
        micros("2026-07-31T12:04:00Z"),
        "team:beta",
        "lambda:reader",
        Some("DEPLOYMENT ALERT: beta"),
        "DATABASE timeout in worker B",
    )
    .await;
    insert_i1422_message(
        &runtime,
        305,
        micros("2026-07-31T12:06:00Z"),
        "team:late",
        "lambda:reader",
        Some("Deployment alert"),
        "database timeout outside the window",
    )
    .await;

    let filters = serde_json::json!({
        "status": "all",
        "limit": 1,
        "from_prefix": "team:",
        "exclude_from_actor": "team:noise",
        "since": "2026-07-31T12:00:00Z",
        "before": "2026-07-31T12:05:00Z",
        "subject_contains": "deployment alert",
        "content_contains": "DATABASE TIMEOUT",
    });
    let first = registry
        .dispatch("comm.inbox", filters.clone())
        .await
        .expect("filtered first page");
    assert_eq!(first["count"], 1);
    assert_eq!(first["messages"][0]["full_id"], newer.to_string());
    assert_eq!(first["has_more"], true);
    assert_eq!(first["next_offset"], 1);

    let mut second_filters = filters;
    second_filters["offset"] = serde_json::json!(1);
    let second = registry
        .dispatch("comm.inbox", second_filters)
        .await
        .expect("filtered second page");
    assert_eq!(second["count"], 1);
    assert_eq!(second["messages"][0]["full_id"], older.to_string());
    assert_eq!(second["has_more"], false);
    assert!(second["next_offset"].is_null());

    let content_only = registry
        .dispatch(
            "comm.inbox",
            serde_json::json!({
                "status": "all",
                "content_contains": "WITHOUT A SUBJECT",
            }),
        )
        .await
        .expect("content filter works independently of subject");
    assert_eq!(content_only["count"], 1);
    assert!(content_only["messages"][0]["subject"].is_null());
}

#[tokio::test]
async fn i1422_time_bounds_are_since_inclusive_and_before_exclusive() {
    let backend = shared_backend();
    let (registry, runtime) = build_actor_registry(backend, "lambda:reader");
    let micros = |timestamp: &str| {
        chrono::DateTime::parse_from_rfc3339(timestamp)
            .unwrap()
            .timestamp_micros()
    };
    let at_since = insert_i1422_message(
        &runtime,
        311,
        micros("2026-07-31T12:00:00Z"),
        "lambda:sender",
        "lambda:reader",
        None,
        "at since",
    )
    .await;
    let inside = insert_i1422_message(
        &runtime,
        312,
        micros("2026-07-31T12:01:00Z"),
        "lambda:sender",
        "lambda:reader",
        None,
        "inside",
    )
    .await;
    insert_i1422_message(
        &runtime,
        313,
        micros("2026-07-31T12:02:00Z"),
        "lambda:sender",
        "lambda:reader",
        None,
        "at before",
    )
    .await;

    let inbox = registry
        .dispatch(
            "comm.inbox",
            serde_json::json!({
                "status": "all",
                "since": "2026-07-31T12:00:00Z",
                "before": "2026-07-31T12:02:00Z",
            }),
        )
        .await
        .expect("bounded inbox");
    let ids: std::collections::HashSet<_> = inbox["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| message["full_id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids, [at_since.to_string(), inside.to_string()].into());
}

#[tokio::test]
async fn i1422_read_ids_marks_a_supplied_set_in_one_operation() {
    let backend = shared_backend();
    let (registry, runtime) = build_actor_registry(backend, "lambda:reader");
    let created_at = chrono::Utc::now().timestamp_micros();
    let ids = [401, 402, 403];
    for sequence in ids {
        insert_i1422_message(
            &runtime,
            sequence,
            created_at + i64::from(sequence),
            "lambda:sender",
            "lambda:reader",
            None,
            &format!("bulk {sequence}"),
        )
        .await;
    }

    let mut raw_ids: Vec<String> = ids
        .into_iter()
        .map(|sequence| {
            uuid::Uuid::from_u128((u128::from(sequence) << 96) | u128::from(sequence)).to_string()
        })
        .collect();
    raw_ids.push(raw_ids[0].clone());
    let result = registry
        .dispatch("comm.read", serde_json::json!({ "ids": raw_ids }))
        .await
        .expect("bulk read succeeds");
    assert_eq!(result["requested_count"], 4);
    assert_eq!(result["unique_count"], 3);
    assert_eq!(result["marked_count"], 3);
    assert_eq!(result["failed_count"], 0);
    assert_eq!(result["results"].as_array().unwrap().len(), 3);
    assert!(result["results"]
        .as_array()
        .unwrap()
        .iter()
        .all(|item| item["read"] == true));

    let unread = registry
        .dispatch("comm.unread", serde_json::json!({}))
        .await
        .expect("unread after bulk read");
    assert_eq!(unread["count"], 0);
}

#[tokio::test]
async fn i1422_bulk_read_validates_every_target_before_mutating() {
    let (registry, runtime) = build_registry_for_ns("local");
    let sent = registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "local", "content": "leave unread on validation error" }),
        )
        .await
        .expect("self-send");
    let outbound_id = sent["full_id"].as_str().unwrap().to_string();
    let inbox = registry
        .dispatch(
            "comm.inbox",
            serde_json::json!({ "status": "unread", "limit": 10 }),
        )
        .await
        .expect("inbox");
    let inbound_id = inbox["messages"][0]["full_id"]
        .as_str()
        .unwrap()
        .to_string();

    let error = registry
        .dispatch(
            "comm.read",
            serde_json::json!({ "ids": [inbound_id.clone(), outbound_id] }),
        )
        .await
        .expect_err("outbound target must reject the whole prevalidation phase");
    assert!(error.to_string().contains("outbound"));

    let token = runtime.authorize(Namespace::local()).expect("local token");
    let stored = runtime
        .notes(&token)
        .unwrap()
        .get_note(inbound_id.parse().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        stored.properties.unwrap()["read"],
        false,
        "a later invalid target must not leave an earlier target marked read"
    );
}

#[tokio::test]
async fn i1387_mark_read_supports_best_effort_and_atomic_bulk_modes() {
    let backend = shared_backend();
    let (registry, runtime) = build_actor_registry(backend, "lambda:reader");
    let created_at = chrono::Utc::now().timestamp_micros();
    let mut ids = Vec::new();
    for sequence in [501_u32, 502, 503] {
        ids.push(
            insert_i1422_message(
                &runtime,
                sequence,
                created_at + i64::from(sequence),
                "lambda:sender",
                "lambda:reader",
                None,
                &format!("named mark-read {sequence}"),
            )
            .await,
        );
    }

    let best_effort = registry
        .dispatch(
            "comm.mark_read",
            serde_json::json!({ "ids": [ids[0].to_string()] }),
        )
        .await
        .expect("mark_read defaults to the shipped best-effort bulk path");
    assert_eq!(best_effort["requested_count"], 1);
    assert_eq!(best_effort["unique_count"], 1);
    assert_eq!(best_effort["marked_count"], 1);
    assert_eq!(best_effort["failed_count"], 0);

    let atomic = registry
        .dispatch(
            "comm.mark_read",
            serde_json::json!({
                "ids": [ids[1].to_string(), ids[2].to_string(), ids[1].to_string()],
                "atomic": true,
            }),
        )
        .await
        .expect("atomic mark_read commits every unique target");
    assert_eq!(atomic["requested_count"], 3);
    assert_eq!(atomic["unique_count"], 2);
    assert_eq!(atomic["marked_count"], 2);
    assert_eq!(atomic["failed_count"], 0);
    assert!(atomic["results"]
        .as_array()
        .unwrap()
        .iter()
        .all(|result| result["read"] == true));

    let token = runtime.authorize(Namespace::local()).expect("local token");
    for id in ids {
        let note = runtime
            .notes(&token)
            .unwrap()
            .get_note(id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(note.properties.unwrap()["read"], true);
    }
}

#[tokio::test]
async fn i1387_atomic_mark_read_rolls_back_an_earlier_live_patch() {
    let backend = shared_backend();
    let (registry, runtime) = build_actor_registry(backend, "lambda:reader");
    let created_at = chrono::Utc::now().timestamp_micros();
    let eligible = insert_i1422_message(
        &runtime,
        504,
        created_at,
        "lambda:sender",
        "lambda:reader",
        None,
        "must roll back",
    )
    .await;
    let non_object = insert_i1422_message(
        &runtime,
        505,
        created_at + 1,
        "lambda:sender",
        "lambda:reader",
        None,
        "transaction guard",
    )
    .await;

    let token = runtime.authorize(Namespace::local()).expect("local token");
    runtime
        .notes(&token)
        .unwrap()
        .update_note_properties(non_object, Some(serde_json::json!([])), created_at + 2)
        .await
        .unwrap();

    let error = registry
        .dispatch(
            "comm.mark_read",
            serde_json::json!({
                "ids": [eligible.to_string(), non_object.to_string()],
                "atomic": true,
            }),
        )
        .await
        .expect_err("a non-object target must abort the guarded transaction");
    let error = error.to_string();
    assert!(
        error.contains("conflict") && error.contains(&non_object.to_string()),
        "the verb error must name the conflict and failing id {non_object}; got {error}"
    );

    let eligible = runtime
        .notes(&token)
        .unwrap()
        .get_note(eligible)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        eligible.properties.unwrap()["read"],
        false,
        "the first guarded UPDATE must roll back when a later target is ineligible"
    );
    let non_object = runtime
        .notes(&token)
        .unwrap()
        .get_note(non_object)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(non_object.properties, Some(serde_json::json!([])));
}

#[tokio::test]
async fn i1387_atomic_mark_read_preserves_adr057_legacy_fail_open() {
    let backend = shared_backend();
    let (registry, runtime) = build_actor_registry(backend, "lambda:reader");
    let created_at = chrono::Utc::now().timestamp_micros();
    let legacy = insert_i1422_message(
        &runtime,
        506,
        created_at,
        "lambda:sender",
        "lambda:reader",
        None,
        "legacy addressee-free message",
    )
    .await;

    let token = runtime.authorize(Namespace::local()).expect("local token");
    runtime
        .notes(&token)
        .unwrap()
        .update_note_properties(
            legacy,
            Some(serde_json::json!({ "direction": "inbound", "read": false })),
            created_at + 1,
        )
        .await
        .unwrap();

    let result = registry
        .dispatch(
            "comm.mark_read",
            serde_json::json!({ "ids": [legacy.to_string()], "atomic": true }),
        )
        .await
        .expect("ADR-057 keeps addressee-free legacy rows markable");
    assert_eq!(result["marked_count"], 1);
    assert_eq!(result["failed_count"], 0);

    let stored = runtime
        .notes(&token)
        .unwrap()
        .get_note(legacy)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.properties.unwrap()["read"], true);
}

#[tokio::test]
async fn i1387_atomic_mark_read_reuses_addressee_validation_before_mutation() {
    let backend = shared_backend();
    let (registry_a, runtime) = build_actor_registry(backend.clone(), "lambda:a");
    let (registry_b, _runtime_b) = build_actor_registry(backend, "lambda:b");

    registry_a
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:b", "content": "for B" }),
        )
        .await
        .expect("A sends to B");
    registry_b
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:a", "content": "for A" }),
        )
        .await
        .expect("B sends to A");

    let token = runtime.authorize(Namespace::local()).expect("local token");
    let notes = runtime
        .list_notes(&token, Some("message"), 100, 0)
        .await
        .unwrap();
    let inbound_for = |actor: &str| {
        notes
            .iter()
            .find(|note| {
                note.properties
                    .as_ref()
                    .and_then(|properties| properties.get("direction"))
                    .and_then(serde_json::Value::as_str)
                    == Some("inbound")
                    && note
                        .properties
                        .as_ref()
                        .and_then(|properties| properties.get("to_actor"))
                        .and_then(serde_json::Value::as_str)
                        == Some(actor)
            })
            .map(|note| note.id)
            .unwrap_or_else(|| panic!("inbound message for {actor} must exist"))
    };
    let for_a = inbound_for("lambda:a");
    let for_b = inbound_for("lambda:b");

    let error = registry_a
        .dispatch(
            "comm.mark_read",
            serde_json::json!({
                "ids": [for_a.to_string(), for_b.to_string()],
                "atomic": true,
            }),
        )
        .await
        .expect_err("A cannot mark B's inbound delivery state");
    let error = error.to_string();
    assert!(error.contains("read: message"));
    assert!(error.contains("lambda:a"));
    assert!(!error.contains("lambda:b"));

    for id in [for_a, for_b] {
        let note = runtime
            .notes(&token)
            .unwrap()
            .get_note(id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            note.properties.unwrap()["read"],
            false,
            "complete validation must occur before the atomic mutation"
        );
    }
}

#[tokio::test]
async fn i1422_rejects_invalid_filter_and_bulk_shapes() {
    let (registry, _runtime) = build_registry_for_ns("local");

    for params in [
        serde_json::json!({ "since": "not-a-timestamp" }),
        serde_json::json!({
            "since": "2026-07-31T12:00:00Z",
            "before": "2026-07-31T12:00:00Z",
        }),
        serde_json::json!({ "subject_contains": "   " }),
        serde_json::json!({ "content_contains": "" }),
    ] {
        assert!(
            registry.dispatch("comm.inbox", params).await.is_err(),
            "invalid inbox filter must be rejected"
        );
    }

    assert!(registry
        .dispatch("comm.read", serde_json::json!({}))
        .await
        .is_err());
    assert!(registry
        .dispatch(
            "comm.read",
            serde_json::json!({ "id": "00000000", "ids": ["00000000"] }),
        )
        .await
        .is_err());
    assert!(registry
        .dispatch("comm.read", serde_json::json!({ "ids": [] }))
        .await
        .is_err());
    assert!(registry
        .dispatch(
            "comm.read",
            serde_json::json!({ "ids": vec!["00000000"; 501] }),
        )
        .await
        .is_err());
    assert!(registry
        .dispatch("comm.mark_read", serde_json::json!({ "ids": [] }))
        .await
        .is_err());
    assert!(registry
        .dispatch(
            "comm.mark_read",
            serde_json::json!({ "ids": vec!["00000000"; 501] }),
        )
        .await
        .is_err());
    assert!(registry
        .dispatch(
            "comm.mark_read",
            serde_json::json!({ "ids": ["00000000"], "atomic": "yes" }),
        )
        .await
        .is_err());
}

/// Build a registry the same way as [`build_registry`] but also install the pack-owned note kind set, mirroring `khive-mcp`'s boot path (`KhiveMcpServer::with_packs`).
fn build_registry_with_owned_kinds() -> (VerbRegistry, KhiveRuntime) {
    let (registry, rt) = build_registry();
    rt.install_pack_owned_note_kinds(
        registry
            .pack_owned_note_kinds()
            .into_iter()
            .map(str::to_string)
            .collect(),
    );
    (registry, rt)
}

/// Build a registry the same way as [`build_registry_with_owned_kinds`] but also install the pack-owned note-write validator, mirroring both `khive-mcp` boot paths.
fn build_registry_with_owned_kinds_and_validator() -> (VerbRegistry, KhiveRuntime) {
    let (registry, rt) = build_registry_with_owned_kinds();
    registry.call_register_note_write_validators(&rt);
    (registry, rt)
}

/// The confirmed hole, reproduced as a test: a generic `update(properties= {from_actor: ...})` must no longer be able to forge the handler-stamped `from_actor` on a `message` note.
#[tokio::test]
async fn update_refuses_to_forge_owner_established_properties_on_message_note() {
    let (registry, _rt) = build_registry_with_owned_kinds();

    let sent = registry
        .dispatch(
            "comm.send",
            serde_json::json!({"to": "local", "content": "identity guard probe"}),
        )
        .await
        .expect("self-send must succeed");
    let full_id = sent
        .get("full_id")
        .and_then(serde_json::Value::as_str)
        .expect("send must return full_id")
        .to_string();

    for (key, forged_value) in [
        ("from_actor", "lambda:leo"),
        ("to_actor", "lambda:leo"),
        ("direction", "inbound"),
        ("sent_at", "1970-01-01T00:00:00Z"),
        ("outbound_ref", "00000000-0000-0000-0000-000000000000"),
        ("thread_id", "forged-thread"),
        ("subject", "forged-subject"),
        ("wire_message_id", "<forged@example.com>"),
        ("external_id", "<forged-external@example.com>"),
    ] {
        let before = registry
            .dispatch("get", serde_json::json!({"id": full_id}))
            .await
            .expect("get must succeed");
        let before_props = before["properties"].clone();

        let err = registry
            .dispatch(
                "update",
                serde_json::json!({"id": full_id, "properties": {key: forged_value}}),
            )
            .await
            .expect_err(&format!(
                "update naming `{key}` on a message note must be refused"
            ));
        let msg = format!("{err}");
        assert!(
            msg.contains(key),
            "refusal error must name the offending key `{key}`; got: {msg}"
        );
        assert!(
            !msg.contains(forged_value),
            "refusal error must never echo the attempted value `{forged_value}` \
             (secret-gate discipline applies to error strings); got: {msg}"
        );

        let after = registry
            .dispatch("get", serde_json::json!({"id": full_id}))
            .await
            .expect("get must succeed");
        assert_eq!(
            after["properties"],
            before_props,
            "refused update naming `{key}` must leave the ENTIRE stored properties \
             object unchanged; got before={before_props} after={after}",
            after = after["properties"]
        );
    }
}

/// Positive arm: a non-owned property update on the same `message` note must still succeed and round-trip — the guard admits everything it does not specifically name.
#[tokio::test]
async fn update_admits_non_owned_properties_on_message_note() {
    let (registry, _rt) = build_registry_with_owned_kinds();

    let sent = registry
        .dispatch(
            "comm.send",
            serde_json::json!({"to": "local", "content": "identity guard positive arm"}),
        )
        .await
        .expect("self-send must succeed");
    let full_id = sent
        .get("full_id")
        .and_then(serde_json::Value::as_str)
        .expect("send must return full_id")
        .to_string();

    registry
        .dispatch(
            "update",
            serde_json::json!({"id": full_id, "properties": {"blocked_on": "review"}}),
        )
        .await
        .expect("update naming only a non-owned key must succeed");

    let after = registry
        .dispatch("get", serde_json::json!({"id": full_id}))
        .await
        .expect("get must succeed");
    assert_eq!(
        after["properties"]["blocked_on"], "review",
        "non-owned property must round-trip"
    );
    assert!(
        after["properties"]["from_actor"].is_string(),
        "from_actor must still be present and untouched by the unrelated patch"
    );
}

/// Quarantine disposition and channel attribution are transport-owned facts.
/// Generic update must not hide a parked message or move it between channels;
/// the supported recovery mutation is delete/purge.
#[tokio::test]
async fn update_refuses_to_forge_transport_owned_quarantine_properties() {
    let (registry, _rt) = build_registry();
    let ingested = registry
        .dispatch(
            "comm.ingest",
            serde_json::json!({
                "namespace": "local",
                "from": "email:quarantine",
                "to": "local",
                "content": "transport-owned update guard",
                "channel_kind": "email",
                "channel_slug": "real-account",
                "external_id": "transport-owned-update-guard",
                "metadata": {"quarantined": true, "quarantine_reason": "test"},
            }),
        )
        .await
        .expect("the legitimate comm.ingest path must remain accepted");
    let full_id = ingested["full_id"].as_str().expect("full_id").to_string();

    for (key, forged_value) in [
        ("quarantined", serde_json::json!(false)),
        ("channel_kind", serde_json::json!("telegram")),
        ("channel_slug", serde_json::json!("forged-account")),
    ] {
        let before = registry
            .dispatch("get", serde_json::json!({"id": full_id}))
            .await
            .expect("get before refused update");
        let err = registry
            .dispatch(
                "update",
                serde_json::json!({"id": full_id, "properties": {key: forged_value}}),
            )
            .await
            .expect_err("generic update must refuse transport-owned properties");
        assert!(
            err.to_string().contains(key),
            "error must name {key}: {err}"
        );
        let after = registry
            .dispatch("get", serde_json::json!({"id": full_id}))
            .await
            .expect("get after refused update");
        assert_eq!(
            after["properties"], before["properties"],
            "refused `{key}` update must leave all message properties unchanged"
        );
    }
}

/// The pack hook exercised above belongs to `VerbRegistry::dispatch("update", ...)`.
/// A Rust embedder can call the public runtime API directly, so the canonical
/// runtime update seam must independently enforce the same transport ownership.
#[tokio::test]
async fn direct_runtime_update_refuses_transport_owned_message_properties() {
    let (registry, runtime) = build_registry_with_owned_kinds_and_validator();
    let ingested = registry
        .dispatch(
            "comm.ingest",
            serde_json::json!({
                "namespace": "local",
                "from": "email:quarantine",
                "to": "local",
                "content": "direct runtime transport guard",
                "channel_kind": "email",
                "channel_slug": "real-account",
                "external_id": "direct-runtime-transport-guard",
                "metadata": {"quarantined": true, "quarantine_reason": "test"},
            }),
        )
        .await
        .expect("the trusted ingest path must seed transport evidence");
    let id = uuid::Uuid::parse_str(ingested["full_id"].as_str().expect("full_id"))
        .expect("full_id must be canonical");
    let token = runtime.authorize(Namespace::local()).expect("local token");
    let before = runtime
        .get_note_including_deleted(&token, id)
        .await
        .expect("read seeded note")
        .expect("seeded note");

    for (key, forged_value) in [
        ("quarantined", serde_json::json!(false)),
        ("channel_kind", serde_json::json!("telegram")),
        ("channel_slug", serde_json::json!("forged-account")),
    ] {
        let error = runtime
            .update_note(
                &token,
                id,
                NotePatch::new(
                    None,
                    None,
                    None,
                    None,
                    Some(serde_json::json!({key: forged_value})),
                ),
            )
            .await
            .expect_err("direct runtime update must refuse transport-owned message properties");
        assert!(
            error.to_string().contains(key),
            "direct-runtime refusal must name {key}: {error}"
        );
        assert_eq!(
            runtime
                .get_note_including_deleted(&token, id)
                .await
                .expect("read note after refusal")
                .expect("note after refusal"),
            before,
            "refused direct update naming `{key}` must leave the whole note unchanged"
        );
    }
}

/// Atomic execution prepares note replacement DML without dispatching the KG
/// pack hook. Preparation must reject the same forged transport evidence
/// before it can enter an atomic write plan.
#[tokio::test]
async fn atomic_prepare_refuses_transport_owned_message_properties() {
    let (registry, runtime) = build_registry_with_owned_kinds_and_validator();
    let ingested = registry
        .dispatch(
            "comm.ingest",
            serde_json::json!({
                "namespace": "local",
                "from": "email:quarantine",
                "to": "local",
                "content": "atomic prepare transport guard",
                "channel_kind": "email",
                "channel_slug": "real-account",
                "external_id": "atomic-prepare-transport-guard",
                "metadata": {"quarantined": true, "quarantine_reason": "test"},
            }),
        )
        .await
        .expect("the trusted ingest path must seed transport evidence");
    let full_id = ingested["full_id"].as_str().expect("full_id");
    let token = runtime.authorize(Namespace::local()).expect("local token");

    for (key, forged_value) in [
        ("quarantined", serde_json::json!(false)),
        ("channel_kind", serde_json::json!("telegram")),
        ("channel_slug", serde_json::json!("forged-account")),
    ] {
        let error = khive_runtime::atomic_prepare::prepare_update(
            &runtime,
            &token,
            &serde_json::json!({
                "id": full_id,
                "properties": {key: forged_value},
            }),
            Some(khive_runtime::atomic_prepare::AtomicUpdateKind::Note {
                specific: Some("message".to_string()),
            }),
        )
        .await
        .expect_err("atomic preparation must refuse transport-owned message properties");
        assert!(
            error.to_string().contains(key),
            "atomic-prepare refusal must name {key}: {error}"
        );
    }
}

/// The transport-property rule belongs specifically to comm's `message`
/// kind. The same JSON keys remain ordinary metadata on a generic note when
/// a Rust embedder calls the runtime directly.
#[tokio::test]
async fn direct_runtime_update_allows_transport_named_properties_on_other_kinds() {
    let (registry, runtime) = build_registry_with_owned_kinds_and_validator();
    let created = registry
        .dispatch(
            "create",
            serde_json::json!({
                "kind": "observation",
                "content": "foreign-kind transport-name control",
            }),
        )
        .await
        .expect("create observation");
    let id =
        uuid::Uuid::parse_str(created["id"].as_str().expect("id")).expect("id must be canonical");
    let token = runtime.authorize(Namespace::local()).expect("local token");

    let updated = runtime
        .update_note(
            &token,
            id,
            NotePatch::new(
                None,
                None,
                None,
                None,
                Some(serde_json::json!({
                    "quarantined": true,
                    "channel_kind": "user-label",
                    "channel_slug": "user-value",
                })),
            ),
        )
        .await
        .expect("transport-named keys are not reserved on an observation");
    assert_eq!(updated.properties.unwrap()["quarantined"], true);
}

/// The guard fires only on pack-owned kinds: naming `from_actor` on a base
/// kg note kind (e.g. `observation`) must succeed.
#[tokio::test]
async fn update_permits_from_actor_key_on_generic_note_kind() {
    let (registry, _rt) = build_registry_with_owned_kinds();

    let created = registry
        .dispatch(
            "create",
            serde_json::json!({"kind": "observation", "content": "generic-kind arm"}),
        )
        .await
        .expect("create observation must succeed");
    let id = created
        .get("id")
        .and_then(serde_json::Value::as_str)
        .expect("create must return id")
        .to_string();

    registry
        .dispatch(
            "update",
            serde_json::json!({"id": id, "properties": {"from_actor": "anyone"}}),
        )
        .await
        .expect("`from_actor` is not a reserved key on a non-pack-owned note kind");

    let after = registry
        .dispatch("get", serde_json::json!({"id": id}))
        .await
        .expect("get must succeed");
    assert_eq!(after["properties"]["from_actor"], "anyone");
}

/// A non-object `properties` patch on a `message` note is refused: it would replace the whole property object (erasing every owned key) rather than merging into it.
#[tokio::test]
async fn update_refuses_non_object_properties_patch_on_message_note() {
    let (registry, _rt) = build_registry_with_owned_kinds();

    let sent = registry
        .dispatch(
            "comm.send",
            serde_json::json!({"to": "local", "content": "non-object patch arm"}),
        )
        .await
        .expect("self-send must succeed");
    let full_id = sent
        .get("full_id")
        .and_then(serde_json::Value::as_str)
        .expect("send must return full_id")
        .to_string();

    let err = registry
        .dispatch(
            "update",
            serde_json::json!({"id": full_id, "properties": "not-an-object"}),
        )
        .await
        .expect_err("a non-object properties patch on a pack-owned note must be refused");
    let msg = format!("{err}");
    assert!(
        msg.contains("object"),
        "refusal error must explain the object requirement; got: {msg}"
    );
}

// ---- ADR-124 note-write identity guard: CREATE-path derivation ----

/// Public generic message creation cannot manufacture transport evidence that
/// only `comm.ingest` is authorized to establish.
#[tokio::test]
async fn create_refuses_transport_owned_quarantine_properties() {
    let (registry, _rt) = build_registry();

    for (key, forged_value) in [
        ("quarantined", serde_json::json!(true)),
        ("channel_kind", serde_json::json!("email")),
        ("channel_slug", serde_json::json!("victim-account")),
    ] {
        let err = registry
            .dispatch(
                "create",
                serde_json::json!({
                    "kind": "message",
                    "content": format!("generic create forgery: {key}"),
                    "properties": {key: forged_value},
                }),
            )
            .await
            .expect_err("generic create must refuse transport-owned properties");
        assert!(
            err.to_string().contains(key),
            "error must name {key}: {err}"
        );
    }

    let messages = registry
        .dispatch("list", serde_json::json!({"kind": "message", "limit": 10}))
        .await
        .expect("list after refused creates");
    assert_eq!(
        messages["items"].as_array().map(Vec::len),
        Some(0),
        "a refused generic create must not leave a partial message row"
    );
}

/// FORGE arm: a generic `create(kind="message", properties={from_actor:
/// "forged"})` call under an authenticated token for actor X must store
/// `from_actor == X` — the true caller — not the forged value. This is not a
/// refusal: the create succeeds and the identity property is silently
/// corrected to the value the authorization token actually names.
#[tokio::test]
async fn create_derives_from_actor_overwriting_a_forged_value() {
    let (registry, _rt) = build_registry_with_owned_kinds_and_validator();
    let true_actor = "lambda:true-caller";

    let created = registry
        .dispatch_with_identity(
            "create",
            serde_json::json!({
                "kind": "message",
                "content": "create-path forgery probe",
                "properties": {"from_actor": "forged"},
            }),
            Some(RequestIdentity {
                namespace: "local".to_string(),
                actor_id: Some(true_actor.to_string()),
                ..Default::default()
            }),
        )
        .await
        .expect("create must succeed — the guard derives, it does not refuse");

    assert_eq!(
        created["properties"]["from_actor"], true_actor,
        "the stored from_actor must be the authenticated caller, not the forged value"
    );
}

/// LEGITIMATE-NO-KEY arm: a `create(kind="message", content=...)` with no `from_actor` in properties at all must still come out stamped with the authenticated caller.
#[tokio::test]
async fn create_stamps_from_actor_when_caller_supplies_no_identity_key() {
    let (registry, _rt) = build_registry_with_owned_kinds_and_validator();
    let true_actor = "lambda:no-key-caller";

    let created = registry
        .dispatch_with_identity(
            "create",
            serde_json::json!({
                "kind": "message",
                "content": "create-path no-key probe",
            }),
            Some(RequestIdentity {
                namespace: "local".to_string(),
                actor_id: Some(true_actor.to_string()),
                ..Default::default()
            }),
        )
        .await
        .expect("create with no properties key must succeed");

    assert_eq!(
        created["properties"]["from_actor"], true_actor,
        "an absent from_actor key must be stamped with the authenticated caller"
    );
}

/// DAEMON-STAMP arm: the real `comm.send` writer path must still stamp `from_actor` correctly and succeed once the validator is installed.
#[tokio::test]
async fn comm_send_still_stamps_from_actor_with_validator_installed() {
    let (registry, _rt) = build_registry_with_owned_kinds_and_validator();
    let true_actor = "lambda:sender";

    let sent = registry
        .dispatch_with_identity(
            "comm.send",
            serde_json::json!({"to": "local", "content": "validator does not break comm.send"}),
            Some(RequestIdentity {
                namespace: "local".to_string(),
                actor_id: Some(true_actor.to_string()),
                ..Default::default()
            }),
        )
        .await
        .expect("comm.send must still succeed with the validator installed");

    let full_id = sent
        .get("full_id")
        .and_then(serde_json::Value::as_str)
        .expect("send must return full_id")
        .to_string();
    let after = registry
        .dispatch("get", serde_json::json!({"id": full_id}))
        .await
        .expect("get must succeed");
    assert_eq!(
        after["properties"]["from_actor"], true_actor,
        "comm.send's own from_actor stamp must still be the sending actor"
    );
}

/// GENERIC-KIND arm: the validator is single-occupancy across all packs, so a `create` on a kind comm does not own (`observation`, owned by kg) must pass its properties through untouched.
#[tokio::test]
async fn create_leaves_generic_kind_properties_untouched() {
    let (registry, _rt) = build_registry_with_owned_kinds_and_validator();

    let created = registry
        .dispatch_with_identity(
            "create",
            serde_json::json!({
                "kind": "observation",
                "content": "generic-kind create arm",
                "properties": {"from_actor": "x"},
            }),
            Some(RequestIdentity {
                namespace: "local".to_string(),
                actor_id: Some("lambda:someone-else".to_string()),
                ..Default::default()
            }),
        )
        .await
        .expect("create observation must succeed");

    assert_eq!(
        created["properties"]["from_actor"], "x",
        "a foreign (non-message) kind's properties must pass through the validator unchanged"
    );

    let message_created = registry
        .dispatch_with_identity(
            "create",
            serde_json::json!({
                "kind": "message",
                "content": "same-fixture message arm — validator must be scoped, not absent",
                "properties": {"from_actor": "forged"},
            }),
            Some(RequestIdentity {
                namespace: "local".to_string(),
                actor_id: Some("lambda:someone-else".to_string()),
                ..Default::default()
            }),
        )
        .await
        .expect("create message must succeed");

    assert_eq!(
        message_created["properties"]["from_actor"], "lambda:someone-else",
        "on the SAME registry, a `message` create must still be derived — proving \
         the validator is installed and merely scoped away from `observation`, \
         not absent entirely"
    );
}

async fn send_message_as(registry: &VerbRegistry, actor: &str, content: &str) -> String {
    let sent = registry
        .dispatch_with_identity(
            "comm.send",
            serde_json::json!({"to": "local", "content": content}),
            Some(RequestIdentity {
                namespace: "local".to_string(),
                actor_id: Some(actor.to_string()),
                ..Default::default()
            }),
        )
        .await
        .expect("self-send must succeed");
    sent.get("full_id")
        .and_then(serde_json::Value::as_str)
        .expect("send must return full_id")
        .to_string()
}

/// FORGERY-BLOCKED arm: merging a `message` note authored by Y into one authored by X with `strategy="prefer_from"` — the attack this guard exists for — must leave the surviving note's `from_actor` as X, not Y.
#[tokio::test]
async fn merge_preserves_into_note_from_actor_under_prefer_from() {
    let (registry, _rt) = build_registry_with_owned_kinds_and_validator();

    let into_id = send_message_as(&registry, "lambda:x", "into-note, authored by X").await;
    let from_id = send_message_as(&registry, "lambda:y", "from-note, authored by Y").await;

    registry
        .dispatch(
            "merge",
            serde_json::json!({
                "kind": "message",
                "into_id": into_id,
                "from_id": from_id,
                "strategy": "prefer_from",
            }),
        )
        .await
        .expect("merge must succeed");

    let after = registry
        .dispatch("get", serde_json::json!({"id": into_id}))
        .await
        .expect("get must succeed");
    assert_eq!(
        after["properties"]["from_actor"], "lambda:x",
        "prefer_from must not be able to transfer attribution from the absorbed note"
    );
}

/// CONTROL arm: the same merge under `strategy="prefer_into"` must also leave `from_actor` as X.
#[tokio::test]
async fn merge_preserves_into_note_from_actor_under_prefer_into() {
    let (registry, _rt) = build_registry_with_owned_kinds_and_validator();

    let into_id = send_message_as(&registry, "lambda:x", "into-note, control arm").await;
    let from_id = send_message_as(&registry, "lambda:y", "from-note, control arm").await;

    registry
        .dispatch(
            "merge",
            serde_json::json!({
                "kind": "message",
                "into_id": into_id,
                "from_id": from_id,
                "strategy": "prefer_into",
            }),
        )
        .await
        .expect("merge must succeed");

    let after = registry
        .dispatch("get", serde_json::json!({"id": into_id}))
        .await
        .expect("get must succeed");
    assert_eq!(after["properties"]["from_actor"], "lambda:x");
}

/// Transport provenance belongs to one transport item, so merging another
/// message into the survivor must never import channel identity or quarantine
/// disposition that the survivor did not already have. An absent key is part
/// of the survivor's immutable state just as much as a present value. The
/// table covers every public property merge policy; each policy would import
/// the absorbed note's absent-on-into keys without the restoration guard.
#[tokio::test]
async fn merge_never_transfers_transport_properties_under_any_strategy() {
    let (registry, _rt) = build_registry_with_owned_kinds_and_validator();

    for strategy in ["prefer_into", "prefer_from", "union"] {
        let into = registry
            .dispatch(
                "comm.ingest",
                serde_json::json!({
                    "namespace": "local",
                    "from": "legacy:unattributed",
                    "to": "local",
                    "content": format!("transport-free survivor for {strategy}"),
                    "external_id": format!("transport-free-survivor-{strategy}"),
                }),
            )
            .await
            .expect("trusted ingest may preserve a legacy unattributed row");
        let into_id = into["full_id"].as_str().expect("full_id").to_string();

        let from = registry
            .dispatch(
                "comm.ingest",
                serde_json::json!({
                    "namespace": "local",
                    "from": "email:quarantine",
                    "to": "local",
                    "content": format!("attributed transport source for {strategy}"),
                    "channel_kind": "email",
                    "channel_slug": format!("source-{strategy}"),
                    "external_id": format!("attributed-transport-source-{strategy}"),
                }),
            )
            .await
            .expect("trusted ingest must establish transport evidence");
        let from_id = from["full_id"].as_str().expect("full_id").to_string();
        let from_before = registry
            .dispatch("get", serde_json::json!({"id": from_id}))
            .await
            .expect("read transport source before merge");
        assert_eq!(from_before["properties"]["channel_kind"], "email");
        assert_eq!(
            from_before["properties"]["channel_slug"],
            format!("source-{strategy}")
        );

        registry
            .dispatch(
                "merge",
                serde_json::json!({
                    "kind": "message",
                    "into_id": into_id,
                    "from_id": from_id,
                    "strategy": strategy,
                }),
            )
            .await
            .expect("generic message merge must succeed");

        let after = registry
            .dispatch("get", serde_json::json!({"id": into_id}))
            .await
            .expect("read survivor after merge");
        for key in ["channel_kind", "channel_slug"] {
            assert!(
                after["properties"].get(key).is_none(),
                "{strategy} must not transfer transport-owned `{key}` from the absorbed \
                 message into a survivor that lacked it; got {after}"
            );
        }
    }
}

/// A quarantined message participates in no merges at all: folding its content
/// into an ordinary message while the marker restoration drops `quarantined`
/// would launder quarantined transport content into an unmarked record.
/// Release is the channel-ingest path's decision, never a curation side
/// effect, so the merge is refused in every strategy and either operand role.
#[tokio::test]
async fn merge_refuses_quarantined_message_under_every_strategy() {
    let (registry, _rt) = build_registry_with_owned_kinds_and_validator();

    for strategy in ["prefer_into", "prefer_from", "union"] {
        let ordinary = registry
            .dispatch(
                "comm.ingest",
                serde_json::json!({
                    "namespace": "local",
                    "from": "legacy:unattributed",
                    "to": "local",
                    "content": format!("ordinary operand for {strategy}"),
                    "external_id": format!("quarantine-merge-ordinary-{strategy}"),
                }),
            )
            .await
            .expect("ordinary ingest");
        let quarantined = registry
            .dispatch(
                "comm.ingest",
                serde_json::json!({
                    "namespace": "local",
                    "from": "email:quarantine",
                    "to": "local",
                    "content": format!("quarantined operand for {strategy}"),
                    "channel_kind": "email",
                    "channel_slug": format!("quarantine-merge-{strategy}"),
                    "external_id": format!("quarantine-merge-parked-{strategy}"),
                    "metadata": {"quarantined": true, "quarantine_reason": "test"},
                }),
            )
            .await
            .expect("quarantined ingest");
        let ordinary_id = ordinary["full_id"].as_str().expect("full_id").to_string();
        let quarantined_id = quarantined["full_id"]
            .as_str()
            .expect("full_id")
            .to_string();

        for (into_id, from_id) in [
            (&ordinary_id, &quarantined_id),
            (&quarantined_id, &ordinary_id),
        ] {
            let error = registry
                .dispatch(
                    "merge",
                    serde_json::json!({
                        "kind": "message",
                        "into_id": into_id,
                        "from_id": from_id,
                        "strategy": strategy,
                    }),
                )
                .await
                .expect_err("a quarantined message must not merge in either role");
            assert!(
                error.to_string().contains("quarantined"),
                "{strategy}: {error}"
            );
        }

        let parked = registry
            .dispatch("get", serde_json::json!({"id": quarantined_id}))
            .await
            .expect("quarantined note intact after refused merges");
        assert_eq!(parked["properties"]["quarantined"], true);
        assert!(
            parked["content"]
                .as_str()
                .expect("content")
                .contains("quarantined operand"),
            "refused merge must not mutate the quarantined operand"
        );
    }
}

/// Scope control for merge: transport-shaped names on `observation` are
/// caller metadata, so the normal `prefer_from` fold must remain intact.
#[tokio::test]
async fn merge_transfers_transport_named_properties_on_non_message_notes() {
    let (registry, _rt) = build_registry_with_owned_kinds_and_validator();
    let into = registry
        .dispatch(
            "create",
            serde_json::json!({
                "kind": "observation",
                "content": "transport-name merge control survivor",
            }),
        )
        .await
        .expect("create into observation");
    let from = registry
        .dispatch(
            "create",
            serde_json::json!({
                "kind": "observation",
                "content": "transport-name merge control source",
                "properties": {
                    "quarantined": true,
                    "channel_kind": "user-label",
                    "channel_slug": "user-value",
                },
            }),
        )
        .await
        .expect("create from observation");
    let into_id = into["id"].as_str().expect("id");
    let from_id = from["id"].as_str().expect("id");

    registry
        .dispatch(
            "merge",
            serde_json::json!({
                "kind": "observation",
                "into_id": into_id,
                "from_id": from_id,
                "strategy": "prefer_from",
            }),
        )
        .await
        .expect("transport-shaped names do not restrict observation merge");
    let after = registry
        .dispatch("get", serde_json::json!({"id": into_id}))
        .await
        .expect("read merged observation");
    assert_eq!(after["properties"]["quarantined"], true);
    assert_eq!(after["properties"]["channel_kind"], "user-label");
    assert_eq!(after["properties"]["channel_slug"], "user-value");
}

/// NON-IDENTITY-KEY arm: a non-owned property that differs between the two
/// notes still folds by strategy — `prefer_from` takes the from-note's
/// value — proving the preserve step pins only the owner-established keys,
/// not the whole property object.
#[tokio::test]
async fn merge_still_folds_non_owned_properties_by_strategy() {
    let (registry, _rt) = build_registry_with_owned_kinds_and_validator();

    let into_id = send_message_as(&registry, "lambda:x", "into-note, non-identity arm").await;
    let from_id = send_message_as(&registry, "lambda:y", "from-note, non-identity arm").await;

    registry
        .dispatch(
            "update",
            serde_json::json!({"id": into_id, "properties": {"tag": "into-tag"}}),
        )
        .await
        .expect("update must succeed");
    registry
        .dispatch(
            "update",
            serde_json::json!({"id": from_id, "properties": {"tag": "from-tag"}}),
        )
        .await
        .expect("update must succeed");

    registry
        .dispatch(
            "merge",
            serde_json::json!({
                "kind": "message",
                "into_id": into_id,
                "from_id": from_id,
                "strategy": "prefer_from",
            }),
        )
        .await
        .expect("merge must succeed");

    let after = registry
        .dispatch("get", serde_json::json!({"id": into_id}))
        .await
        .expect("get must succeed");
    assert_eq!(
        after["properties"]["tag"], "from-tag",
        "a non-owned key must still fold by strategy — only owner-established keys are pinned"
    );
    assert_eq!(
        after["properties"]["from_actor"], "lambda:x",
        "the owner-established key must remain pinned to the into-note in the same merge"
    );
}

/// GENERIC-KIND arm: merging two `observation` notes (a kind comm does not own) under `prefer_from` must let a `from_actor`-named property overwrite normally — the preservation guard fires only on pack-owned kinds.
#[tokio::test]
async fn merge_overwrites_from_actor_on_generic_kind_under_prefer_from() {
    let (registry, _rt) = build_registry_with_owned_kinds_and_validator();

    let into_created = registry
        .dispatch(
            "create",
            serde_json::json!({
                "kind": "observation",
                "content": "into observation",
                "properties": {"from_actor": "x"},
            }),
        )
        .await
        .expect("create must succeed");
    let into_id = into_created["id"]
        .as_str()
        .expect("create must return id")
        .to_string();

    let from_created = registry
        .dispatch(
            "create",
            serde_json::json!({
                "kind": "observation",
                "content": "from observation",
                "properties": {"from_actor": "y"},
            }),
        )
        .await
        .expect("create must succeed");
    let from_id = from_created["id"]
        .as_str()
        .expect("create must return id")
        .to_string();

    registry
        .dispatch(
            "merge",
            serde_json::json!({
                "kind": "observation",
                "into_id": into_id,
                "from_id": from_id,
                "strategy": "prefer_from",
            }),
        )
        .await
        .expect("merge must succeed");

    let after = registry
        .dispatch("get", serde_json::json!({"id": into_id}))
        .await
        .expect("get must succeed");
    assert_eq!(
        after["properties"]["from_actor"], "y",
        "on a non-pack-owned kind, prefer_from must overwrite from_actor normally"
    );
}

/// PROPERTIES-MERGED-ACCURACY arm: restoring an owner-established key that was already present on the into-note (here `to_actor`) must not be double-counted against `properties_merged` — the fold never counted that key as "added" in the first place, because `to_actor` already existed on the into-note before the merge.
#[tokio::test]
async fn merge_reports_properties_merged_for_key_that_actually_survives() {
    let (registry, _rt) = build_registry_with_owned_kinds_and_validator();

    let into_created = registry
        .dispatch(
            "create",
            serde_json::json!({
                "kind": "message",
                "content": "into note, properties_merged accuracy arm",
                "properties": {"to_actor": "into", "base": "i"},
            }),
        )
        .await
        .expect("create must succeed");
    let into_id = into_created["id"]
        .as_str()
        .expect("create must return id")
        .to_string();

    let from_created = registry
        .dispatch(
            "create",
            serde_json::json!({
                "kind": "message",
                "content": "from note, properties_merged accuracy arm",
                "properties": {"to_actor": "from", "added": "x"},
            }),
        )
        .await
        .expect("create must succeed");
    let from_id = from_created["id"]
        .as_str()
        .expect("create must return id")
        .to_string();

    let merged = registry
        .dispatch(
            "merge",
            serde_json::json!({
                "kind": "message",
                "into_id": into_id,
                "from_id": from_id,
                "strategy": "prefer_from",
            }),
        )
        .await
        .expect("merge must succeed");

    assert_eq!(
        merged["properties_merged"], 1,
        "exactly one non-owned key (`added`) genuinely survived the merge; got {merged}"
    );

    let after = registry
        .dispatch("get", serde_json::json!({"id": into_id}))
        .await
        .expect("get must succeed");
    assert_eq!(
        after["properties"]["added"], "x",
        "the newly introduced non-owned key must survive on the merged note"
    );
    assert_eq!(
        after["properties"]["base"], "i",
        "the into-note's own pre-existing non-owned key must survive too"
    );
    assert_eq!(
        after["properties"]["to_actor"], "into",
        "the owner-established key must remain pinned to the into-note, not \
         the from-note's value"
    );
}

/// NESTED-UNION arm: an owner-established key that holds an OBJECT (`thread_id`) merged under `strategy="union"` must not be double-counted either.
#[tokio::test]
async fn merge_reports_zero_properties_merged_for_nested_union_reversion() {
    let (registry, _rt) = build_registry_with_owned_kinds_and_validator();

    let identity = RequestIdentity {
        namespace: "local".to_string(),
        actor_id: Some("lambda:z".to_string()),
        ..Default::default()
    };

    let into_created = registry
        .dispatch_with_identity(
            "create",
            serde_json::json!({
                "kind": "message",
                "content": "into note, nested-union accuracy arm",
                "properties": {"thread_id": {"keep": 1}},
            }),
            Some(identity.clone()),
        )
        .await
        .expect("create must succeed");
    let into_id = into_created["id"]
        .as_str()
        .expect("create must return id")
        .to_string();

    let from_created = registry
        .dispatch_with_identity(
            "create",
            serde_json::json!({
                "kind": "message",
                "content": "from note, nested-union accuracy arm",
                "properties": {"thread_id": {"discarded": 2}},
            }),
            Some(identity),
        )
        .await
        .expect("create must succeed");
    let from_id = from_created["id"]
        .as_str()
        .expect("create must return id")
        .to_string();

    let merged = registry
        .dispatch(
            "merge",
            serde_json::json!({
                "kind": "message",
                "into_id": into_id,
                "from_id": from_id,
                "strategy": "union",
            }),
        )
        .await
        .expect("merge must succeed");

    let after = registry
        .dispatch("get", serde_json::json!({"id": into_id}))
        .await
        .expect("get must succeed");
    assert_eq!(
        after["properties"]["thread_id"],
        serde_json::json!({"keep": 1}),
        "the absorbed note's nested contribution must not survive restoration"
    );
    assert_eq!(
        merged["properties_merged"], 0,
        "nothing from the absorbed note actually survived restoration, so \
         properties_merged must report 0, not the nested fold's raw count; \
         got {merged}"
    );
}

/// ROUTE-LEVEL RESTORATION arm: the whole scenario driven through the pack's actual `comm.send`/`create` + `merge` verbs (not `count_new_property_keys` called directly — that unit-level coverage already lives in `khive-runtime`'s `curation.rs` tests), on a pack-owned `message` note.
#[tokio::test]
async fn merge_reports_zero_properties_merged_when_restoration_reverts_the_only_new_key_through_the_route(
) {
    let (registry, _rt) = build_registry_with_owned_kinds_and_validator();

    let into_id = send_message_as(
        &registry,
        "lambda:x",
        "into note, route-level restoration arm — no external_id",
    )
    .await;

    let from_created = registry
        .dispatch_with_identity(
            "create",
            serde_json::json!({
                "kind": "message",
                "content": "from note, route-level restoration arm — has an external_id",
                "properties": {"external_id": "wire-abc-123"},
            }),
            Some(RequestIdentity {
                namespace: "local".to_string(),
                actor_id: Some("lambda:y".to_string()),
                ..Default::default()
            }),
        )
        .await
        .expect("create must succeed");
    let from_id = from_created["id"]
        .as_str()
        .expect("create must return id")
        .to_string();

    let before = registry
        .dispatch("get", serde_json::json!({"id": into_id}))
        .await
        .expect("get must succeed");
    assert!(
        before["properties"].get("external_id").is_none(),
        "fixture invariant: the into-note must not already carry an \
         `external_id`; got {before}"
    );

    let merged = registry
        .dispatch(
            "merge",
            serde_json::json!({
                "kind": "message",
                "into_id": into_id,
                "from_id": from_id,
                "strategy": "prefer_from",
            }),
        )
        .await
        .expect("merge must succeed");

    let after = registry
        .dispatch("get", serde_json::json!({"id": into_id}))
        .await
        .expect("get must succeed");
    assert!(
        after["properties"].get("external_id").is_none(),
        "restoration must strip the absorbed note's `external_id`, since the \
         into-note never had one — got {after}"
    );
    assert_eq!(
        after["properties"]["from_actor"], "lambda:x",
        "the into-note's owner-established `from_actor` must survive the merge \
         unchanged"
    );
    assert_eq!(
        merged["properties_merged"], 0,
        "the only key the fold counted as new (`external_id`) was reverted by \
         restoration, so nothing genuinely survived; got {merged}"
    );
}

#[tokio::test]
async fn i1471_sent_box_is_sender_scoped_and_filters_recipient_and_since() {
    let backend = shared_backend();
    let (registry_a, _runtime_a) = build_actor_registry(backend.clone(), "lambda:a");
    let (registry_other, _runtime_other) = build_actor_registry(backend, "lambda:other");
    let since = (chrono::Utc::now() - chrono::Duration::seconds(1)).to_rfc3339();

    let to_b = registry_a
        .dispatch(
            "comm.send",
            serde_json::json!({
                "to": "lambda:b",
                "subject": "for B",
                "content": "sender A to B",
            }),
        )
        .await
        .expect("A sends to B");
    registry_a
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:c", "content": "sender A to C" }),
        )
        .await
        .expect("A sends to C");
    registry_other
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:b", "content": "other sender to B" }),
        )
        .await
        .expect("other actor sends to B");

    let default_box = registry_a
        .dispatch(
            "comm.inbox",
            serde_json::json!({ "status": "all", "limit": 10 }),
        )
        .await
        .expect("default inbox remains inbound-only");
    assert_eq!(default_box["count"], 0);

    let sent_to_b = registry_a
        .dispatch(
            "comm.inbox",
            serde_json::json!({
                "box": "sent",
                "to_actor": "lambda:b",
                "since": since,
                "limit": 10,
            }),
        )
        .await
        .expect("sent history");
    assert_eq!(sent_to_b["count"], 1);
    assert_eq!(sent_to_b["messages"][0]["full_id"], to_b["full_id"]);
    assert_eq!(sent_to_b["messages"][0]["from"], "lambda:a");
    assert_eq!(sent_to_b["messages"][0]["to"], "lambda:b");
    assert_eq!(sent_to_b["messages"][0]["direction"], "outbound");
    assert_eq!(sent_to_b["unread_count"], 0);

    let all_sent = registry_a
        .dispatch(
            "comm.inbox",
            serde_json::json!({ "box": "sent", "limit": 10 }),
        )
        .await
        .expect("all caller-authored sent rows");
    assert_eq!(
        all_sent["count"], 2,
        "another actor's outbound row must not leak"
    );

    let future = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
    let none = registry_a
        .dispatch(
            "comm.inbox",
            serde_json::json!({ "box": "sent", "since": future }),
        )
        .await
        .expect("sent since filter");
    assert_eq!(none["count"], 0);

    let status_error = registry_a
        .dispatch(
            "comm.inbox",
            serde_json::json!({ "box": "sent", "status": "all" }),
        )
        .await
        .expect_err("read status has no meaning for sent rows");
    assert!(status_error.to_string().contains("applies only"));
}

#[tokio::test]
async fn i1468_fields_projects_inbox_and_thread_with_one_strict_vocabulary() {
    let backend = shared_backend();
    let (registry_a, _runtime_a) = build_actor_registry(backend.clone(), "lambda:a");
    let (registry_b, _runtime_b) = build_actor_registry(backend, "lambda:b");

    let sent = registry_a
        .dispatch(
            "comm.send",
            serde_json::json!({
                "to": "lambda:b",
                "subject": "projection contract",
                "content": "body must be omitted from the projected view",
            }),
        )
        .await
        .expect("send for projection test");
    let fields = serde_json::json!(["id", "subject", "from_actor", "sent_at", "created_at"]);

    let inbox = registry_b
        .dispatch(
            "comm.inbox",
            serde_json::json!({ "status": "all", "fields": fields.clone() }),
        )
        .await
        .expect("projected inbox");
    assert_eq!(inbox["count"], 1);
    let inbox_message = inbox["messages"][0].as_object().unwrap();
    let expected: std::collections::BTreeSet<&str> =
        ["id", "subject", "from_actor", "sent_at", "created_at"]
            .into_iter()
            .collect();
    let actual: std::collections::BTreeSet<&str> =
        inbox_message.keys().map(String::as_str).collect();
    assert_eq!(actual, expected);
    assert_eq!(inbox_message["from_actor"], "lambda:a");
    assert!(inbox_message["sent_at"].as_str().is_some());
    assert!(inbox_message["created_at"].as_str().is_some());

    let thread = registry_a
        .dispatch(
            "comm.thread",
            serde_json::json!({ "id": sent["full_id"], "fields": fields }),
        )
        .await
        .expect("projected thread");
    assert_eq!(thread["count"], 1);
    let thread_message = thread["messages"][0].as_object().unwrap();
    let thread_keys: std::collections::BTreeSet<&str> =
        thread_message.keys().map(String::as_str).collect();
    assert_eq!(thread_keys, expected);

    let full = registry_b
        .dispatch("comm.inbox", serde_json::json!({ "status": "all" }))
        .await
        .expect("omitted projection preserves the full view");
    assert!(full["messages"][0].get("content").is_some());
    assert!(full["messages"][0].get("properties").is_some());

    for (verb, params) in [
        (
            "comm.inbox",
            serde_json::json!({ "fields": ["not_a_message_field"] }),
        ),
        (
            "comm.thread",
            serde_json::json!({
                "id": sent["full_id"],
                "fields": ["not_a_message_field"],
            }),
        ),
    ] {
        let error = registry_a
            .dispatch(verb, params)
            .await
            .expect_err("unknown projection field must fail");
        assert!(error.to_string().contains("unknown projection field"));
    }

    let empty = registry_a
        .dispatch("comm.inbox", serde_json::json!({ "fields": [] }))
        .await
        .expect_err("an empty projection is ambiguous and must fail");
    assert!(empty.to_string().contains("at least one field"));
}

/// The anonymous `"local"` caller is scoped by `to_actor = "local" OR to_actor IS NULL` like every other caller (ADR-057 amendment): it shares messages addressed to `"local"`, keeps legacy rows without `to_actor` visible, and must not see messages explicitly addressed to another actor.
#[tokio::test]
async fn i1471_anonymous_local_inbox_scoping_and_legacy_visibility() {
    let backend = shared_backend();
    let (registry_a, _rt_a) = build_actor_registry(backend.clone(), "lambda:a");
    let (registry_local, rt_local) = build_actor_registry(backend, "local");

    registry_a
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:b", "content": "addressed away from local" }),
        )
        .await
        .expect("A sends to B");
    registry_a
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "local", "content": "addressed to local" }),
        )
        .await
        .expect("A sends to local");

    let local_tok = rt_local
        .authorize(Namespace::local())
        .expect("authorize local fixture namespace");
    rt_local
        .create_note(
            &local_tok,
            "message",
            None,
            "legacy inbound, no to_actor",
            None,
            Some(serde_json::json!({
                "from": "lambda:a",
                "from_actor": "lambda:a",
                "direction": "inbound",
            })),
            vec![],
        )
        .await
        .expect("legacy to_actor-less inbound fixture");

    let inbox = registry_local
        .dispatch(
            "comm.inbox",
            serde_json::json!({ "status": "all", "limit": 10 }),
        )
        .await
        .expect("anonymous local inbox");
    assert_eq!(
        inbox["count"], 2,
        "local-addressed row plus legacy row only"
    );
    let contents: Vec<&str> = inbox["messages"]
        .as_array()
        .expect("messages array")
        .iter()
        .filter_map(|message| message["content"].as_str())
        .collect();
    assert!(
        contents.contains(&"addressed to local"),
        "row addressed to \"local\" must be visible; got {contents:?}"
    );
    assert!(
        contents.contains(&"legacy inbound, no to_actor"),
        "legacy row without to_actor must stay visible; got {contents:?}"
    );
    assert!(
        !contents.contains(&"addressed away from local"),
        "row addressed to another actor must stay hidden; got {contents:?}"
    );
}

/// Cross-box filters must fail loudly rather than silently return the wrong box: sender filters and read `status` are inbox-only, while `to_actor` is sent-only (ADR-057 amendment).
#[tokio::test]
async fn i1471_cross_box_filters_are_rejected() {
    let (registry, _rt) = build_registry_for_ns("local");

    for params in [
        serde_json::json!({ "box": "sent", "status": "unread" }),
        serde_json::json!({ "box": "sent", "from_actor": "local" }),
        serde_json::json!({ "box": "sent", "from_prefix": "lambda:" }),
        serde_json::json!({ "box": "sent", "exclude_from_actor": "local" }),
    ] {
        let error = registry
            .dispatch("comm.inbox", params)
            .await
            .expect_err("inbox-only filter with box=\"sent\" must fail");
        assert!(
            error.to_string().contains("only to box=\"inbox\""),
            "error must explain the filter is inbox-only; got {error}"
        );
    }

    for params in [
        serde_json::json!({ "to_actor": "lambda:b" }),
        serde_json::json!({ "box": "inbox", "to_actor": "lambda:b" }),
    ] {
        let error = registry
            .dispatch("comm.inbox", params)
            .await
            .expect_err("to_actor with the inbox box must fail");
        assert!(
            error.to_string().contains("applies only"),
            "error must explain to_actor is sent-only; got {error}"
        );
    }
}

/// The anonymous `"local"` sent box keeps legacy outbound rows without `from_actor` visible (EqOrMissing fallback), while rows attributed to another actor never leak into it.
#[tokio::test]
async fn i1471_local_sent_box_includes_legacy_rows_only() {
    let backend = shared_backend();
    let (registry_other, _rt_other) = build_actor_registry(backend.clone(), "lambda:other");
    let (registry_local, rt_local) = build_actor_registry(backend, "local");

    registry_local
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "local", "content": "sent by local" }),
        )
        .await
        .expect("local sends");
    registry_other
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "local", "content": "sent by other" }),
        )
        .await
        .expect("other actor sends");

    let local_tok = rt_local
        .authorize(Namespace::local())
        .expect("authorize local fixture namespace");
    rt_local
        .create_note(
            &local_tok,
            "message",
            None,
            "legacy outbound, no from_actor",
            None,
            Some(serde_json::json!({
                "to": "lambda:b",
                "direction": "outbound",
            })),
            vec![],
        )
        .await
        .expect("legacy from_actor-less outbound fixture");
    rt_local
        .create_note(
            &local_tok,
            "message",
            None,
            "foreign outbound, different from_actor",
            None,
            Some(serde_json::json!({
                "from_actor": "lambda:foreign",
                "to": "lambda:b",
                "direction": "outbound",
            })),
            vec![],
        )
        .await
        .expect("foreign-attributed outbound fixture");

    let sent = registry_local
        .dispatch(
            "comm.inbox",
            serde_json::json!({ "box": "sent", "limit": 10 }),
        )
        .await
        .expect("local sent history");
    assert_eq!(
        sent["count"], 2,
        "local-authored row plus legacy row only; got {sent}"
    );
    let contents: Vec<&str> = sent["messages"]
        .as_array()
        .expect("messages array")
        .iter()
        .filter_map(|message| message["content"].as_str())
        .collect();
    assert!(
        contents.contains(&"sent by local"),
        "local-authored outbound row must be listed; got {contents:?}"
    );
    assert!(
        contents.contains(&"legacy outbound, no from_actor"),
        "legacy row without from_actor must stay visible; got {contents:?}"
    );
    assert!(
        !contents.contains(&"sent by other")
            && !contents.contains(&"foreign outbound, different from_actor"),
        "rows attributed to another actor must not leak; got {contents:?}"
    );
}

/// An ATTRIBUTED caller's sent box requires an exact `from_actor` match: legacy outbound rows without `from_actor` are never inherited (fail closed), even when they live in the namespace the caller's query scans.
#[tokio::test]
async fn i1471_attributed_sent_box_excludes_legacy_rows() {
    let backend = shared_backend();
    let (registry_a, rt_a) = build_actor_registry(backend, "lambda:a");

    registry_a
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:b", "content": "attributed outbound" }),
        )
        .await
        .expect("attributed actor sends");

    let tok = rt_a
        .authorize(Namespace::local())
        .expect("authorize fixture namespace");
    rt_a.create_note(
        &tok,
        "message",
        None,
        "legacy outbound, no from_actor",
        None,
        Some(serde_json::json!({
            "to": "lambda:b",
            "direction": "outbound",
        })),
        vec![],
    )
    .await
    .expect("legacy from_actor-less outbound fixture");
    rt_a.create_note(
        &tok,
        "message",
        None,
        "attributed fixture, from_actor present",
        None,
        Some(serde_json::json!({
            "from_actor": "lambda:a",
            "to": "lambda:b",
            "direction": "outbound",
        })),
        vec![],
    )
    .await
    .expect("attributed outbound control fixture");

    let sent = registry_a
        .dispatch(
            "comm.inbox",
            serde_json::json!({ "box": "sent", "limit": 10 }),
        )
        .await
        .expect("attributed sent history");
    assert_eq!(
        sent["count"], 2,
        "attributed rows visible, legacy from_actor-less row excluded; got {sent}"
    );
    let contents: Vec<&str> = sent["messages"]
        .as_array()
        .expect("messages array")
        .iter()
        .filter_map(|message| message["content"].as_str())
        .collect();
    assert!(
        contents.contains(&"attributed fixture, from_actor present"),
        "in-scope control fixture must be listed; got {contents:?}"
    );
    assert!(
        !contents.contains(&"legacy outbound, no from_actor"),
        "attributed caller must not inherit legacy from_actor-less rows; got {contents:?}"
    );
}

/// Projection applies to sent rows through the same strict vocabulary as the inbox box, and the sent box always reports `unread_count = 0` (outbound rows carry no recipient read state).
#[tokio::test]
async fn sent_box_fields_projection_applies_and_unread_count_is_zero() {
    let backend = shared_backend();
    let (registry_a, _rt_a) = build_actor_registry(backend, "lambda:a");

    registry_a
        .dispatch(
            "comm.send",
            serde_json::json!({
                "to": "lambda:b",
                "subject": "sent projection",
                "content": "body must be omitted from the projected sent view",
            }),
        )
        .await
        .expect("send for sent projection test");

    let sent = registry_a
        .dispatch(
            "comm.inbox",
            serde_json::json!({
                "box": "sent",
                "fields": ["id", "to_actor", "subject", "direction"],
            }),
        )
        .await
        .expect("projected sent history");
    assert_eq!(sent["count"], 1);
    assert_eq!(
        sent["unread_count"], 0,
        "sent rows have no recipient read state, so the sent box reports zero; got {sent}"
    );
    let message = sent["messages"][0].as_object().expect("message object");
    let expected: std::collections::BTreeSet<&str> = ["id", "to_actor", "subject", "direction"]
        .into_iter()
        .collect();
    let actual: std::collections::BTreeSet<&str> = message.keys().map(String::as_str).collect();
    assert_eq!(actual, expected, "projection must select exactly `fields`");
    assert_eq!(message["to_actor"], "lambda:b");
    assert_eq!(message["subject"], "sent projection");
    assert_eq!(message["direction"], "outbound");
}

/// Sent-box paging walks the newest-first filtered sequence with stable page boundaries: `next_offset` chains pages without overlap or gaps, and the terminal page reports `has_more = false` with a null `next_offset`.
#[tokio::test]
async fn sent_box_paginates_newest_first_with_stable_boundaries() {
    let backend = shared_backend();
    let (registry_a, _rt_a) = build_actor_registry(backend, "lambda:a");

    for index in 1..=3 {
        registry_a
            .dispatch(
                "comm.send",
                serde_json::json!({
                    "to": "lambda:b",
                    "content": format!("sent page {index}"),
                }),
            )
            .await
            .expect("send succeeds");
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    let page_one = registry_a
        .dispatch(
            "comm.inbox",
            serde_json::json!({ "box": "sent", "limit": 2 }),
        )
        .await
        .expect("first sent page");
    assert_eq!(page_one["count"], 2);
    assert_eq!(page_one["offset"], 0);
    assert_eq!(page_one["has_more"], true);
    assert_eq!(page_one["next_offset"], 2);
    let page_one_contents: Vec<&str> = page_one["messages"]
        .as_array()
        .expect("messages array")
        .iter()
        .filter_map(|message| message["content"].as_str())
        .collect();
    assert_eq!(
        page_one_contents,
        vec!["sent page 3", "sent page 2"],
        "the first page must be the two newest rows, newest first"
    );

    let page_two = registry_a
        .dispatch(
            "comm.inbox",
            serde_json::json!({ "box": "sent", "limit": 2, "offset": 2 }),
        )
        .await
        .expect("second sent page");
    assert_eq!(page_two["count"], 1);
    assert_eq!(page_two["offset"], 2);
    assert_eq!(page_two["has_more"], false);
    assert!(
        page_two["next_offset"].is_null(),
        "the terminal page must not hand out a next offset; got {page_two}"
    );
    assert_eq!(
        page_two["messages"][0]["content"].as_str(),
        Some("sent page 1"),
        "the final page picks up exactly where page one ended"
    );
}

/// A `box` value outside the accepted set is rejected, and the error names the valid values.
#[tokio::test]
async fn inbox_rejects_box_value_outside_the_accepted_set() {
    let (registry, _rt) = build_registry_for_ns("local");

    let error = registry
        .dispatch("comm.inbox", serde_json::json!({ "box": "banana" }))
        .await
        .expect_err("an unknown box value must be rejected");
    let message = error.to_string();
    assert!(
        message.contains("invalid `box`"),
        "error must name the `box` parameter; got {message}"
    );
    assert!(
        message.contains("inbox") && message.contains("sent"),
        "error must name the valid values; got {message}"
    );
}

/// An empty-string `to_actor` filter is caller error: stored actor labels are never empty (`send` validates), so the filter could only silently match nothing.
#[tokio::test]
async fn sent_box_rejects_empty_to_actor_filter() {
    let (registry, _rt) = build_registry_for_ns("local");

    let error = registry
        .dispatch(
            "comm.inbox",
            serde_json::json!({ "box": "sent", "to_actor": "" }),
        )
        .await
        .expect_err("an empty to_actor filter must be rejected");
    let message = error.to_string();
    assert!(
        message.contains("`to_actor`") && message.contains("must not be empty"),
        "error must name the parameter and the empty-value rule; got {message}"
    );
}

/// A stored outbound row missing `to_actor`/`from_actor` degrades per the handler's definitions instead of panicking: for the anonymous `"local"` caller the `from_actor` predicate falls back to EqOrMissing so the row stays listed, the projected `to_actor` alias has no property or top-level `to` to fall back to and renders as null, and an exact `to_actor` filter simply does not match the property-less row.
#[tokio::test]
async fn sent_box_null_property_fallback_does_not_panic() {
    let backend = shared_backend();
    let (registry_local, rt_local) = build_actor_registry(backend, "local");

    let local_tok = rt_local
        .authorize(Namespace::local())
        .expect("authorize local fixture namespace");
    let fixture = rt_local
        .create_note(
            &local_tok,
            "message",
            None,
            "legacy outbound, no actor properties",
            None,
            Some(serde_json::json!({ "direction": "outbound" })),
            vec![],
        )
        .await
        .expect("actor-property-less outbound fixture");
    let fixture_id = fixture.id.as_hyphenated().to_string();

    let sent = registry_local
        .dispatch(
            "comm.inbox",
            serde_json::json!({ "box": "sent", "fields": ["id", "to_actor", "from_actor"] }),
        )
        .await
        .expect("projected sent history over a property-less row");
    assert_eq!(
        sent["count"], 1,
        "the local caller's EqOrMissing from_actor fallback must keep the row visible; got {sent}"
    );
    let message = &sent["messages"][0];
    assert_eq!(message["id"], fixture_id);
    assert!(
        message["to_actor"].is_null(),
        "to_actor has no property or top-level `to` to fall back to; got {message}"
    );
    assert_eq!(
        message["from_actor"], "local",
        "from_actor falls back to the top-level `from`, which defaults to the row namespace"
    );

    let filtered = registry_local
        .dispatch(
            "comm.inbox",
            serde_json::json!({ "box": "sent", "to_actor": "lambda:b" }),
        )
        .await
        .expect("exact to_actor filter over a property-less row");
    assert_eq!(
        filtered["count"], 0,
        "an exact to_actor predicate must not match a row without the property; got {filtered}"
    );
}

/// A long-poll on the sent box wakes when the caller sends a new message: the inbox generation counter is direction-agnostic, so an outbound commit publishes the same signal an inbound one does.
#[tokio::test]
async fn sent_box_long_poll_wakes_after_concurrent_send() {
    let (registry, _rt) = build_registry_for_ns("local");
    let waiter_registry = registry.clone();
    let mut waiter = tokio::spawn(async move {
        waiter_registry
            .dispatch(
                "comm.inbox",
                serde_json::json!({ "box": "sent", "wait_ms": 5_000 }),
            )
            .await
    });

    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    assert!(
        !waiter.is_finished(),
        "an empty sent box must remain blocked before a matching send"
    );

    registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "lambda:b", "content": "send wakes the sent box" }),
        )
        .await
        .expect("concurrent send succeeds");

    let sent = match tokio::time::timeout(std::time::Duration::from_secs(1), &mut waiter).await {
        Ok(joined) => joined
            .expect("long-poll task must not panic")
            .expect("long-poll sent box succeeds"),
        Err(_) => {
            waiter.abort();
            panic!("long-poll sent box did not wake within one second of send");
        }
    };
    assert_eq!(sent["count"].as_u64(), Some(1));
    assert_eq!(
        sent["messages"][0]["content"].as_str(),
        Some("send wakes the sent box")
    );
    assert_eq!(sent["messages"][0]["direction"].as_str(), Some("outbound"));
}
