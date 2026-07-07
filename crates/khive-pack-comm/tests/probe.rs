//! Tests for `comm.probe` — the frozen read-only polling contract added by
//! the daemon hardening slice (ADR-D5).
//!
//! INLINE TEST JUSTIFICATION: separate from `tests/integration.rs` because
//! every test here needs `idx_comm_message_to_actor` actually created via
//! `VerbRegistry::apply_schema_plans` (the probe SQL uses `INDEXED BY`, which
//! errors loudly if the index is absent) — `integration.rs`'s shared
//! `build_registry()` fixture intentionally does not apply schema plans, and
//! changing it would be a behavior change for unrelated tests in that file.

use khive_pack_comm::CommPack;
use khive_runtime::{KhiveRuntime, Namespace, VerbRegistry, VerbRegistryBuilder};
use khive_storage::note::Note;
use serde_json::json;
use uuid::Uuid;

/// Build a registry with the comm pack's auxiliary schema plan actually
/// applied, so `idx_comm_message_to_actor` exists for `INDEXED BY` to find.
fn build_registry() -> (VerbRegistry, KhiveRuntime) {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(CommPack::new(runtime.clone()));
    let registry = builder.build().expect("registry builds");

    // The `notes` table is created lazily on first access; schema-plan index
    // DDL requires it to already exist.
    let token = runtime
        .authorize(Namespace::local())
        .expect("authorize local namespace");
    let _ = runtime.notes(&token).expect("notes store");
    registry.apply_schema_plans(runtime.backend());

    (registry, runtime)
}

/// Plant an inbound `message` note directly with an explicit `created_at`,
/// so cursor/ordering/staleness tests are deterministic instead of depending
/// on real-clock resolution between calls.
#[allow(clippy::too_many_arguments)]
async fn plant_inbound_message(
    rt: &KhiveRuntime,
    to_actor: &str,
    from_actor: &str,
    created_at_us: i64,
    subject: Option<&str>,
    read: bool,
) -> Uuid {
    let token = rt
        .authorize(Namespace::local())
        .expect("authorize local namespace");
    let store = rt.notes(&token).expect("notes store");

    let mut properties = json!({
        "direction": "inbound",
        "to_actor": to_actor,
        "from_actor": from_actor,
        "read": read,
    });
    if let Some(subject) = subject {
        properties["subject"] = json!(subject);
    }

    let id = Uuid::new_v4();
    let note = Note {
        id,
        namespace: "local".into(),
        kind: "message".into(),
        status: "active".into(),
        name: None,
        content: "probe test message".into(),
        salience: None,
        decay_factor: None,
        expires_at: None,
        properties: Some(properties),
        created_at: created_at_us,
        updated_at: created_at_us,
        deleted_at: None,
    };
    store.upsert_note(note).await.expect("upsert planted note");
    id
}

#[tokio::test]
async fn probe_empty_inbox_returns_zeroed_response() {
    let (registry, _rt) = build_registry();

    let result = registry
        .dispatch("comm.probe", json!({ "actor": "lambda:nobody" }))
        .await
        .expect("probe succeeds on empty inbox");

    assert_eq!(result["cursor_us"], json!(0));
    assert_eq!(result["new_messages"], json!([]));
    assert_eq!(result["stale_unread_count"], json!(0));
}

#[tokio::test]
async fn probe_cursor_advances_and_filters_since_us() {
    let (registry, rt) = build_registry();
    let actor = "lambda:leo";

    let t1 = 1_000_000_i64;
    let t2 = 2_000_000_i64;
    let id2 = plant_inbound_message(&rt, actor, "lambda:khive", t2, None, false).await;
    plant_inbound_message(&rt, actor, "lambda:khive", t1, None, false).await;

    let result = registry
        .dispatch("comm.probe", json!({ "actor": actor, "since_us": t1 }))
        .await
        .expect("probe succeeds");

    assert_eq!(
        result["cursor_us"],
        json!(t2),
        "cursor is the max created_at"
    );
    let messages = result["new_messages"].as_array().expect("array");
    assert_eq!(
        messages.len(),
        1,
        "only the message strictly newer than since_us is returned: {messages:?}"
    );
    assert_eq!(messages[0]["id"], json!(id2.to_string()));
    assert_eq!(messages[0]["created_at_us"], json!(t2));
}

#[tokio::test]
async fn probe_new_messages_ordered_newest_last() {
    let (registry, rt) = build_registry();
    let actor = "lambda:leo";

    let t1 = 1_000_000_i64;
    let t2 = 2_000_000_i64;
    let t3 = 3_000_000_i64;
    plant_inbound_message(&rt, actor, "a", t2, None, false).await;
    plant_inbound_message(&rt, actor, "a", t1, None, false).await;
    plant_inbound_message(&rt, actor, "a", t3, None, false).await;

    let result = registry
        .dispatch("comm.probe", json!({ "actor": actor }))
        .await
        .expect("probe succeeds");

    let messages = result["new_messages"].as_array().expect("array");
    let timestamps: Vec<i64> = messages
        .iter()
        .map(|m| m["created_at_us"].as_i64().unwrap())
        .collect();
    assert_eq!(
        timestamps,
        vec![t1, t2, t3],
        "new_messages must be ascending (newest-last)"
    );
}

#[tokio::test]
async fn probe_caps_new_messages_at_100_newest() {
    let (registry, rt) = build_registry();
    let actor = "lambda:leo";

    // Plant 105 rows; the oldest 5 must be dropped by the LIMIT 100, and the
    // 100 kept must still come back ascending (oldest-of-the-kept first).
    let base = 1_000_000_i64;
    for i in 0..105 {
        plant_inbound_message(&rt, actor, "a", base + i * 1_000, None, false).await;
    }

    let result = registry
        .dispatch("comm.probe", json!({ "actor": actor }))
        .await
        .expect("probe succeeds");

    let messages = result["new_messages"].as_array().expect("array");
    assert_eq!(messages.len(), 100, "response must cap at 100 messages");

    let timestamps: Vec<i64> = messages
        .iter()
        .map(|m| m["created_at_us"].as_i64().unwrap())
        .collect();
    let expected_first = base + 5 * 1_000; // the 5 oldest rows were dropped
    let expected_last = base + 104 * 1_000;
    assert_eq!(timestamps.first().copied(), Some(expected_first));
    assert_eq!(timestamps.last().copied(), Some(expected_last));
    assert!(
        timestamps.windows(2).all(|w| w[0] < w[1]),
        "kept messages must be strictly ascending: {timestamps:?}"
    );
}

#[tokio::test]
async fn probe_stale_unread_count_uses_default_20_minutes() {
    let (registry, rt) = build_registry();
    let actor = "lambda:leo";

    let now_us = chrono::Utc::now().timestamp_micros();
    let old_unread = now_us - 25 * 60_000_000; // 25 min ago, unread -> stale
    let recent_unread = now_us - 5 * 60_000_000; // 5 min ago, unread -> not stale
    let old_read = now_us - 30 * 60_000_000; // 30 min ago, but read -> not stale

    plant_inbound_message(&rt, actor, "a", old_unread, None, false).await;
    plant_inbound_message(&rt, actor, "a", recent_unread, None, false).await;
    plant_inbound_message(&rt, actor, "a", old_read, None, true).await;

    let result = registry
        .dispatch("comm.probe", json!({ "actor": actor }))
        .await
        .expect("probe succeeds");

    assert_eq!(
        result["stale_unread_count"],
        json!(1),
        "only the old+unread message counts as stale: {result}"
    );
}

#[tokio::test]
async fn probe_respects_custom_stale_minutes() {
    let (registry, rt) = build_registry();
    let actor = "lambda:leo";

    let now_us = chrono::Utc::now().timestamp_micros();
    let ten_min_ago = now_us - 10 * 60_000_000;
    plant_inbound_message(&rt, actor, "a", ten_min_ago, None, false).await;

    // Default (20 min) sees it as fresh.
    let default_result = registry
        .dispatch("comm.probe", json!({ "actor": actor }))
        .await
        .expect("probe succeeds");
    assert_eq!(default_result["stale_unread_count"], json!(0));

    // A 5-minute threshold sees the same message as stale.
    let tight_result = registry
        .dispatch("comm.probe", json!({ "actor": actor, "stale_minutes": 5 }))
        .await
        .expect("probe succeeds");
    assert_eq!(tight_result["stale_unread_count"], json!(1));
}

/// Total `message`-kind notes currently stored, via a direct count query.
async fn count_message_notes(rt: &KhiveRuntime) -> i64 {
    let sql = rt.sql();
    let mut reader = sql.reader().await.expect("reader");
    match reader
        .query_scalar(khive_storage::types::SqlStatement {
            sql: "SELECT COUNT(*) FROM notes WHERE kind = 'message'".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("count query")
    {
        Some(khive_storage::types::SqlValue::Integer(v)) => v,
        other => panic!("expected an integer count, got {other:?}"),
    }
}

/// The JSON type of `properties.$.read` for the single planted message
/// addressed to `actor` (`"true"`, `"false"`, or absent), via `json_type` —
/// the same idiom `handle_inbox` uses to distinguish a real boolean `true`
/// from missing/false/other values.
async fn read_flag_json_type(rt: &KhiveRuntime, actor: &str) -> Option<String> {
    let sql = rt.sql();
    let mut reader = sql.reader().await.expect("reader");
    let row = reader
        .query_row(khive_storage::types::SqlStatement {
            sql: "SELECT json_type(properties, '$.read') AS read_type FROM notes \
                  WHERE kind = 'message' AND json_extract(properties, '$.to_actor') = ?1"
                .into(),
            params: vec![khive_storage::types::SqlValue::Text(actor.to_string())],
            label: None,
        })
        .await
        .expect("row query");
    row.and_then(|r| match r.get("read_type") {
        Some(khive_storage::types::SqlValue::Text(s)) => Some(s.clone()),
        _ => None,
    })
}

#[tokio::test]
async fn probe_is_strictly_read_only() {
    let (registry, rt) = build_registry();
    let actor = "lambda:leo";

    plant_inbound_message(&rt, actor, "a", 1_000_000, Some("hi"), false).await;

    let count_before = count_message_notes(&rt).await;
    let read_type_before = read_flag_json_type(&rt, actor).await;
    assert_eq!(
        read_type_before.as_deref(),
        Some("false"),
        "planted message must start unread"
    );

    let first = registry
        .dispatch("comm.probe", json!({ "actor": actor }))
        .await
        .expect("probe succeeds");
    let second = registry
        .dispatch("comm.probe", json!({ "actor": actor }))
        .await
        .expect("probe succeeds");
    assert_eq!(
        first, second,
        "two probes with identical params must return identical results"
    );

    let count_after = count_message_notes(&rt).await;
    let read_type_after = read_flag_json_type(&rt, actor).await;

    assert_eq!(
        count_before, count_after,
        "comm.probe must not create or delete any note"
    );
    assert_eq!(
        read_type_before, read_type_after,
        "comm.probe must not mutate the read flag"
    );
}

#[tokio::test]
async fn probe_rejects_empty_actor() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch("comm.probe", json!({ "actor": "" }))
        .await
        .expect_err("empty actor must be rejected");
    assert!(
        err.to_string().contains("actor"),
        "error must mention the offending field: {err}"
    );
}

#[tokio::test]
async fn probe_rejects_non_positive_stale_minutes() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch(
            "comm.probe",
            json!({ "actor": "lambda:leo", "stale_minutes": 0 }),
        )
        .await
        .expect_err("non-positive stale_minutes must be rejected");
    assert!(
        err.to_string().contains("stale_minutes"),
        "error must mention the offending field: {err}"
    );
}

#[tokio::test]
async fn probe_ignores_messages_addressed_to_other_actors() {
    let (registry, rt) = build_registry();

    plant_inbound_message(&rt, "lambda:leo", "a", 1_000_000, None, false).await;
    plant_inbound_message(&rt, "lambda:khive", "a", 2_000_000, None, false).await;

    let result = registry
        .dispatch("comm.probe", json!({ "actor": "lambda:leo" }))
        .await
        .expect("probe succeeds");

    let messages = result["new_messages"].as_array().expect("array");
    assert_eq!(
        messages.len(),
        1,
        "a probe for one actor must not see another actor's inbound messages: {messages:?}"
    );
    assert_eq!(result["cursor_us"], json!(1_000_000));
}

#[tokio::test]
async fn probe_ignores_outbound_messages() {
    let (registry, rt) = build_registry();
    let actor = "lambda:leo";

    // An outbound message stored with `to_actor` set to this actor (e.g. a
    // sent-mail record) must never be counted as an inbound poll result.
    let token = rt
        .authorize(Namespace::local())
        .expect("authorize local namespace");
    let store = rt.notes(&token).expect("notes store");
    let note = Note {
        id: Uuid::new_v4(),
        namespace: "local".into(),
        kind: "message".into(),
        status: "active".into(),
        name: None,
        content: "outbound probe test message".into(),
        salience: None,
        decay_factor: None,
        expires_at: None,
        properties: Some(json!({
            "direction": "outbound",
            "to_actor": actor,
            "from_actor": actor,
            "read": false,
        })),
        created_at: 1_000_000,
        updated_at: 1_000_000,
        deleted_at: None,
    };
    store.upsert_note(note).await.expect("upsert outbound note");

    let result = registry
        .dispatch("comm.probe", json!({ "actor": actor }))
        .await
        .expect("probe succeeds");

    assert_eq!(result["cursor_us"], json!(0));
    assert_eq!(result["new_messages"], json!([]));
    assert_eq!(result["stale_unread_count"], json!(0));
}

#[tokio::test]
async fn probe_includes_subject_when_present_and_omits_when_absent() {
    let (registry, rt) = build_registry();
    let actor = "lambda:leo";

    plant_inbound_message(&rt, actor, "a", 1_000_000, Some("with subject"), false).await;
    plant_inbound_message(&rt, actor, "b", 2_000_000, None, false).await;

    let result = registry
        .dispatch("comm.probe", json!({ "actor": actor }))
        .await
        .expect("probe succeeds");

    let messages = result["new_messages"].as_array().expect("array");
    assert_eq!(messages.len(), 2);

    let with_subject = &messages[0];
    assert_eq!(with_subject["created_at_us"], json!(1_000_000));
    assert_eq!(with_subject["from_actor"], json!("a"));
    assert_eq!(with_subject["subject"], json!("with subject"));

    let without_subject = &messages[1];
    assert_eq!(without_subject["created_at_us"], json!(2_000_000));
    assert_eq!(without_subject["from_actor"], json!("b"));
    assert!(
        without_subject.get("subject").is_none(),
        "subject must be omitted (not null) when absent: {without_subject}"
    );
}

#[tokio::test]
async fn probe_rejects_unknown_fields() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch(
            "comm.probe",
            json!({ "actor": "lambda:leo", "typo_field": true }),
        )
        .await
        .expect_err("unknown fields must be rejected, matching the other comm verbs");
    // Just confirm it fails closed; the exact serde message wording is not a
    // stable contract to assert on.
    let _ = err;
}

#[tokio::test]
async fn probe_query_plan_uses_the_to_actor_index() {
    let (_registry, rt) = build_registry();

    let sql = rt.sql();
    let mut reader = sql.reader().await.expect("reader");
    let plan = reader
        .explain(khive_storage::types::SqlStatement {
            sql: "SELECT id FROM notes INDEXED BY idx_comm_message_to_actor \
                  WHERE namespace = ?1 AND kind = 'message' AND deleted_at IS NULL \
                  AND json_extract(properties, '$.to_actor') = ?2 \
                  AND json_extract(properties, '$.direction') = 'inbound'"
                .to_string(),
            params: vec![
                khive_storage::types::SqlValue::Text("local".into()),
                khive_storage::types::SqlValue::Text("lambda:leo".into()),
            ],
            label: Some("comm_probe_plan_check".into()),
        })
        .await
        .expect("EXPLAIN QUERY PLAN succeeds when the index exists");

    let plan_text: String = plan
        .iter()
        .flat_map(|row| row.columns.iter())
        .filter_map(|c| match &c.value {
            khive_storage::types::SqlValue::Text(s) => Some(s.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        plan_text.contains("idx_comm_message_to_actor"),
        "query plan must use idx_comm_message_to_actor: {plan_text}"
    );
}
