//! Tests for `comm.probe` — the frozen read-only polling contract added by
//! the daemon hardening slice (ADR-D5).
//!
//! INLINE TEST JUSTIFICATION: separate from `tests/integration.rs` because
//! every test here needs `idx_comm_message_to_actor` actually created via
//! `VerbRegistry::apply_schema_plans` (the probe SQL uses `INDEXED BY`, which
//! errors loudly if the index is absent) — `integration.rs`'s shared
//! `build_registry()` fixture intentionally does not apply schema plans, and
//! changing it would be a behavior change for unrelated tests in that file.

use std::sync::Arc;

use khive_pack_comm::CommPack;
use khive_runtime::{
    AllowAllGate, BackendId, KhiveRuntime, Namespace, RuntimeConfig, VerbRegistry,
    VerbRegistryBuilder,
};
use khive_storage::note::Note;
use khive_storage::types::DeleteMode;
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
    plant_inbound_message(&rt, actor, "lambda:khive", t1, None, false).await;

    // `cursor_us` is an opaque token round-tripped from a prior probe
    // response (#780) -- not a raw timestamp a caller computes itself.
    let first = registry
        .dispatch("comm.probe", json!({ "actor": actor }))
        .await
        .expect("probe succeeds");
    let cursor_1 = first["cursor_us"]
        .as_i64()
        .expect("cursor_us is an integer");
    assert_eq!(first["new_messages"].as_array().unwrap().len(), 1);

    let id2 = plant_inbound_message(&rt, actor, "lambda:khive", t2, None, false).await;

    let second = registry
        .dispatch(
            "comm.probe",
            json!({ "actor": actor, "since_us": cursor_1 }),
        )
        .await
        .expect("probe succeeds");

    assert!(
        second["cursor_us"].as_i64().unwrap() > cursor_1,
        "cursor advances past the previous token"
    );
    let messages = second["new_messages"].as_array().expect("array");
    assert_eq!(
        messages.len(),
        1,
        "only the message planted after cursor_1 is returned: {messages:?}"
    );
    assert_eq!(messages[0]["id"], json!(id2.to_string()));
    assert_eq!(messages[0]["created_at_us"], json!(t2));
}

/// Regression for #780: the probe cursor must be keyed on commit order
/// (the durable `notes_seq.seq`), not the application-clock `created_at` stamped before a
/// write acquires the writer critical section. Two concurrent writers can
/// commit out of stamp order; a `created_at`-keyed cursor then permanently
/// hides whichever row committed second but stamped an earlier clock read.
#[tokio::test]
async fn probe_survives_out_of_order_commit_vs_created_at() {
    let (registry, rt) = build_registry();
    let actor = "lambda:leo";

    let t_high = 5_000_000_i64;
    let t_low = 1_000_000_i64;

    // "Winner" of the writer-lock race: commits FIRST, but its clock read
    // (taken before the race) is LATER.
    let id_winner = plant_inbound_message(&rt, actor, "lambda:khive", t_high, None, false).await;

    let first = registry
        .dispatch("comm.probe", json!({ "actor": actor }))
        .await
        .expect("probe succeeds");
    let cursor_1 = first["cursor_us"]
        .as_i64()
        .expect("cursor_us is an integer");
    let first_messages = first["new_messages"].as_array().expect("array");
    assert!(
        first_messages
            .iter()
            .any(|m| m["id"] == json!(id_winner.to_string())),
        "first probe must see the winner message: {first_messages:?}"
    );

    // "Loser": commits SECOND, but its clock read (taken before it lost the
    // writer-lock race) is EARLIER than the winner's.
    let id_loser = plant_inbound_message(&rt, actor, "lambda:khive", t_low, None, false).await;

    let second = registry
        .dispatch(
            "comm.probe",
            json!({ "actor": actor, "since_us": cursor_1 }),
        )
        .await
        .expect("probe succeeds");
    let second_messages = second["new_messages"].as_array().expect("array");

    // Under the OLD created_at-keyed cursor, t_low <= cursor_1 (t_high) would
    // exclude the loser forever. Under the FIXED commit-order cursor
    // (`notes_seq.seq`), the loser's sequence is still greater than
    // cursor_1's, regardless of its created_at value.
    assert_eq!(
        second_messages.len(),
        1,
        "the loser message must be visible to the next probe, not permanently \
         skipped: {second_messages:?}"
    );
    assert_eq!(second_messages[0]["id"], json!(id_loser.to_string()));
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
    assert!(
        result["cursor_us"].as_i64().unwrap() > 0,
        "cursor must advance for the actor's own message, independent of the other \
         actor's row: {result}"
    );
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

/// Regression for #780's paired hazard: `comm.read` must patch a message's
/// `properties` via a real `UPDATE`, not `upsert_note`'s `INSERT OR REPLACE`
/// (a SQLite DELETE+INSERT on a primary-key conflict, which rewrites the
/// row). The cursor is now keyed on `notes_seq.seq`, which is fixed at first
/// insert and survives such churn, so this guards the defensive in-place
/// invariant: marking an already-probed message as read must never make it
/// resurface as "new" on the next poll.
#[tokio::test]
async fn probe_read_does_not_resurrect_message_via_rowid_churn() {
    let (registry, rt) = build_registry();
    let actor = "lambda:leo";

    let id = plant_inbound_message(&rt, actor, "lambda:khive", 1_000_000, None, false).await;

    // Probe past it: the cursor now covers this message's commit-order key.
    let first = registry
        .dispatch("comm.probe", json!({ "actor": actor }))
        .await
        .expect("probe succeeds");
    let cursor = first["cursor_us"]
        .as_i64()
        .expect("cursor_us is an integer");
    assert_eq!(first["new_messages"].as_array().unwrap().len(), 1);

    registry
        .dispatch("comm.read", json!({ "id": id.to_string() }))
        .await
        .expect("comm.read succeeds");

    let second = registry
        .dispatch("comm.probe", json!({ "actor": actor, "since_us": cursor }))
        .await
        .expect("probe succeeds");
    let second_messages = second["new_messages"].as_array().expect("array");
    assert!(
        second_messages.is_empty(),
        "marking a message read must not resurrect it as new: {second_messages:?}"
    );
}

/// Regression for #827 Finding 1(a): `notes` has a TEXT PRIMARY KEY, so it
/// carries an *implicit* rowid that SQLite may renumber on `VACUUM` (khive
/// exposes `memory.vacuum`). The cursor must survive a VACUUM between probes
/// without losing or replaying any message.
#[tokio::test]
async fn probe_survives_vacuum_between_probes() {
    let (registry, rt) = build_registry();
    let actor = "lambda:leo";

    let t1 = 1_000_000_i64;
    plant_inbound_message(&rt, actor, "lambda:khive", t1, None, false).await;

    let first = registry
        .dispatch("comm.probe", json!({ "actor": actor }))
        .await
        .expect("probe succeeds");
    let cursor_1 = first["cursor_us"]
        .as_i64()
        .expect("cursor_us is an integer");
    assert_eq!(first["new_messages"].as_array().unwrap().len(), 1);

    // VACUUM the whole database between probes. If the cursor were still
    // keyed on `notes`' implicit rowid, VACUUM could renumber it and either
    // lose or replay messages relative to a previously issued cursor.
    let sql = rt.sql();
    {
        let mut writer = sql.writer().await.expect("writer");
        writer
            .execute_script_top_level("VACUUM;".to_string())
            .await
            .expect("vacuum succeeds");
    }

    let t2 = 2_000_000_i64;
    let id2 = plant_inbound_message(&rt, actor, "lambda:khive", t2, None, false).await;

    let second = registry
        .dispatch(
            "comm.probe",
            json!({ "actor": actor, "since_us": cursor_1 }),
        )
        .await
        .expect("probe succeeds");
    let messages = second["new_messages"].as_array().expect("array");
    assert_eq!(
        messages.len(),
        1,
        "VACUUM must not lose or replay messages: {messages:?}"
    );
    assert_eq!(messages[0]["id"], json!(id2.to_string()));

    // Repeating the same `since_us` (simulating a retried poll) must not
    // replay the first message either.
    let replay_check = registry
        .dispatch(
            "comm.probe",
            json!({ "actor": actor, "since_us": cursor_1 }),
        )
        .await
        .expect("probe succeeds");
    let replay_messages = replay_check["new_messages"].as_array().expect("array");
    assert_eq!(
        replay_messages.len(),
        1,
        "the first message must not be replayed after VACUUM: {replay_messages:?}"
    );
    assert_eq!(replay_messages[0]["id"], json!(id2.to_string()));
}

/// Regression for #827 Finding 1(b): SQLite reuses the highest rowid of a
/// plain (non-AUTOINCREMENT) rowid table once that row is deleted. `notes`
/// exposes a public hard delete, so deleting the note that currently holds
/// the highest `notes_seq.seq` and then inserting a new note must not let
/// the new note be silently excluded by a stale cursor.
#[tokio::test]
async fn probe_survives_delete_of_highest_seq_note_before_next_insert() {
    let (registry, rt) = build_registry();
    let actor = "lambda:leo";

    let t1 = 1_000_000_i64;
    let id1 = plant_inbound_message(&rt, actor, "lambda:khive", t1, None, false).await;

    let first = registry
        .dispatch("comm.probe", json!({ "actor": actor }))
        .await
        .expect("probe succeeds");
    let cursor_1 = first["cursor_us"]
        .as_i64()
        .expect("cursor_us is an integer");

    let token = rt
        .authorize(Namespace::local())
        .expect("authorize local namespace");
    let store = rt.notes(&token).expect("notes store");
    let deleted = store
        .delete_note(id1, DeleteMode::Hard)
        .await
        .expect("hard delete succeeds");
    assert!(deleted, "the planted note must exist before deletion");

    let t2 = 2_000_000_i64;
    let id2 = plant_inbound_message(&rt, actor, "lambda:khive", t2, None, false).await;

    let second = registry
        .dispatch(
            "comm.probe",
            json!({ "actor": actor, "since_us": cursor_1 }),
        )
        .await
        .expect("probe succeeds");
    let messages = second["new_messages"].as_array().expect("array");
    assert_eq!(
        messages.len(),
        1,
        "a note inserted after the highest-seq note is hard-deleted must still be \
         visible, not permanently excluded by rowid reuse: {messages:?}"
    );
    assert_eq!(messages[0]["id"], json!(id2.to_string()));
}

/// Regression for #827: the returned cursor must never regress below a
/// caller-supplied `since_us`. `stats.cursor_us` is `MAX(notes_seq.seq)`
/// over the currently-matching rows, so hard-deleting the row that held the
/// highest seq can otherwise make a later probe report a smaller cursor than
/// one it already handed out.
#[tokio::test]
async fn probe_cursor_never_regresses_below_caller_supplied_since_us() {
    let (registry, rt) = build_registry();
    let actor = "lambda:leo";

    let t1 = 1_000_000_i64;
    let t2 = 2_000_000_i64;
    plant_inbound_message(&rt, actor, "lambda:khive", t1, None, false).await;
    let id2 = plant_inbound_message(&rt, actor, "lambda:khive", t2, None, false).await;

    let first = registry
        .dispatch("comm.probe", json!({ "actor": actor }))
        .await
        .expect("probe succeeds");
    let cursor_1 = first["cursor_us"]
        .as_i64()
        .expect("cursor_us is an integer");

    let token = rt
        .authorize(Namespace::local())
        .expect("authorize local namespace");
    let store = rt.notes(&token).expect("notes store");
    store
        .delete_note(id2, DeleteMode::Hard)
        .await
        .expect("hard delete succeeds");

    let second = registry
        .dispatch(
            "comm.probe",
            json!({ "actor": actor, "since_us": cursor_1 }),
        )
        .await
        .expect("probe succeeds");
    assert!(
        second["cursor_us"].as_i64().unwrap() >= cursor_1,
        "cursor must never regress below a previously issued since_us: {second}"
    );
}

/// Regression for #827 Finding 2: a pre-upgrade persisted cursor was a raw
/// Unix-microsecond `created_at` timestamp -- vastly larger than any real
/// `notes_seq` value. Passing one back as `since_us` must reset to baseline
/// instead of permanently suppressing every message.
#[tokio::test]
async fn probe_resets_implausible_pre_upgrade_timestamp_cursor_to_baseline() {
    let (registry, rt) = build_registry();
    let actor = "lambda:leo";

    let t1 = 1_000_000_i64;
    plant_inbound_message(&rt, actor, "lambda:khive", t1, None, false).await;

    let stale_timestamp_cursor = 1_751_932_800_000_000_i64;
    let result = registry
        .dispatch(
            "comm.probe",
            json!({ "actor": actor, "since_us": stale_timestamp_cursor }),
        )
        .await
        .expect("probe succeeds");

    let messages = result["new_messages"].as_array().expect("array");
    assert_eq!(
        messages.len(),
        1,
        "an implausible pre-upgrade timestamp cursor must reset to baseline, not \
         permanently suppress messages: {messages:?}"
    );
}

/// Regression for #827 Finding 3: `notes_seq` has no fixed ceiling on how
/// high a legitimate sequence value can grow. A `since_us` far above the
/// old fixed `1_000_000_000_000` cutoff, but still at or below the actual
/// `notes_seq` high-water mark, must round-trip normally -- not be reset to
/// baseline. This directly exercises `comm.probe`'s opaque round-trip
/// contract (`vocab.rs`): whatever `cursor_us` a probe hands back must work
/// as `since_us` on the next call, at any magnitude.
#[tokio::test]
async fn probe_round_trips_a_legitimately_high_sequence_cursor() {
    let (registry, rt) = build_registry();
    let actor = "lambda:leo";

    // Plant enough messages to push notes_seq comfortably past the old fixed
    // 1_000_000_000_000 cutoff would have been irrelevant at this count, so
    // instead directly advance the `notes_seq` AUTOINCREMENT high-water mark
    // past that bound, then plant one real message after it.
    // The comm pack's notes DDL bootstrap (`notes-ddl.sql`) already registers
    // a `notes_seq` row in `sqlite_sequence` (at `seq = 0`) as a side effect
    // of its own idempotent backfill `INSERT ... SELECT`, even before any
    // real note is ever inserted -- so this advances that existing row via
    // `UPDATE` rather than inserting a second, conflicting one (SQLite's
    // AUTOINCREMENT bookkeeping does not enforce uniqueness on `name` at the
    // SQL layer, so a duplicate row would silently be ignored by the
    // engine's internal lookup).
    let sql = rt.sql();
    {
        let mut writer = sql.writer().await.expect("writer");
        writer
            .execute_script(
                "UPDATE sqlite_sequence SET seq = 2000000000000 WHERE name = 'notes_seq';"
                    .to_string(),
            )
            .await
            .expect("advance notes_seq high-water mark");
    }

    let t1 = 1_000_000_i64;
    plant_inbound_message(&rt, actor, "lambda:khive", t1, None, false).await;

    let first = registry
        .dispatch("comm.probe", json!({ "actor": actor }))
        .await
        .expect("probe succeeds");
    let cursor_1 = first["cursor_us"]
        .as_i64()
        .expect("cursor_us is an integer");
    assert!(
        cursor_1 > 1_000_000_000_000,
        "the planted message's sequence value must exceed the old fixed cutoff: {cursor_1}"
    );
    assert_eq!(first["new_messages"].as_array().unwrap().len(), 1);

    let t2 = 2_000_000_i64;
    let id2 = plant_inbound_message(&rt, actor, "lambda:khive", t2, None, false).await;

    let second = registry
        .dispatch(
            "comm.probe",
            json!({ "actor": actor, "since_us": cursor_1 }),
        )
        .await
        .expect("probe succeeds");
    let messages = second["new_messages"].as_array().expect("array");
    assert_eq!(
        messages.len(),
        1,
        "a legitimately high sequence cursor must round-trip, not be reset to \
         baseline and replay the first message: {messages:?}"
    );
    assert_eq!(messages[0]["id"], json!(id2.to_string()));
}

/// Regression for #827 Finding 1: `V7` (`sql/007-notes-seq.sql`) must
/// backfill `notes_seq` for every note that already existed on a populated
/// V6 database, not just notes inserted after the upgrade. Without the
/// backfill, `comm.probe`'s `INNER JOIN notes_seq` would silently drop every
/// pre-existing inbound message from `new_messages`, `cursor_us`, and
/// `stale_unread_count` forever.
#[tokio::test]
async fn probe_backfills_pre_existing_messages_across_v6_to_v7_upgrade() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("v6_upgrade.db");
    let actor = "lambda:leo";
    let pre_existing_id = Uuid::new_v4();

    // Build a V6-state database directly: apply only migrations up to V6,
    // then insert a pre-existing inbound message the old way -- `notes_seq`
    // does not exist yet at V6, so this note has no sequence row.
    {
        let backend = khive_db::StorageBackend::sqlite(&path).expect("open v6 backend");
        let writer = backend.pool().try_writer().expect("writer");
        let conn = writer.conn();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS _schema_migrations ( \
                 version INTEGER PRIMARY KEY, name TEXT NOT NULL, applied_at INTEGER NOT NULL);",
        )
        .expect("create migration tracking table");

        let now = chrono::Utc::now().timestamp_micros();
        for migration in khive_db::migrations::MIGRATIONS
            .iter()
            .filter(|m| m.version <= 6)
        {
            conn.execute_batch(migration.up)
                .unwrap_or_else(|e| panic!("apply V{}: {e}", migration.version));
            conn.execute(
                "INSERT INTO _schema_migrations (version, name, applied_at) VALUES (?1, ?2, ?3)",
                rusqlite::params![migration.version, migration.name, now],
            )
            .unwrap_or_else(|e| panic!("record V{}: {e}", migration.version));
        }

        conn.execute(
            "INSERT INTO notes \
             (id, namespace, kind, status, name, content, salience, decay_factor, expires_at, \
              properties, created_at, updated_at, deleted_at) \
             VALUES (?1, 'local', 'message', 'active', NULL, 'pre-existing v6 message', \
                     NULL, NULL, NULL, ?2, ?3, ?3, NULL)",
            rusqlite::params![
                pre_existing_id.to_string(),
                json!({
                    "direction": "inbound",
                    "to_actor": actor,
                    "from_actor": "lambda:khive",
                    "read": false,
                })
                .to_string(),
                1_000_000_i64,
            ],
        )
        .expect("insert pre-existing v6 message");
    }

    // Reopen through the normal runtime boot path -- this runs
    // `run_migrations` to latest, including V7's backfill.
    let config = RuntimeConfig {
        db_path: Some(path.clone()),
        default_namespace: Namespace::local(),
        embedding_model: None,
        additional_embedding_models: vec![],
        gate: Arc::new(AllowAllGate),
        packs: vec!["kg".to_string(), "comm".to_string()],
        backend_id: BackendId::main(),
        brain_profile: None,
        visible_namespaces: vec![],
        allowed_outbound_namespaces: vec![],
        actor_id: None,
    };
    let runtime = KhiveRuntime::new(config).expect("runtime reopens and migrates to latest");

    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(CommPack::new(runtime.clone()));
    let registry = builder.build().expect("registry builds");
    registry.apply_schema_plans(runtime.backend());

    let result = registry
        .dispatch("comm.probe", json!({ "actor": actor }))
        .await
        .expect("probe succeeds after v6->v7 upgrade");

    let messages = result["new_messages"].as_array().expect("array");
    assert_eq!(
        messages.len(),
        1,
        "the pre-existing V6 message must be backfilled into notes_seq and appear \
         in the first probe after upgrading to V7: {messages:?}"
    );
    assert_eq!(messages[0]["id"], json!(pre_existing_id.to_string()));
    assert!(
        result["cursor_us"].as_i64().unwrap() > 0,
        "cursor_us must reflect the backfilled pre-existing message: {result}"
    );
}

/// Regression for #827 round-3 Finding 1: the *original* V7 migration (head
/// 87c25939, before round 2 added a backfill) only created `notes_seq` --
/// it never backfilled anything. A database that already ran that original
/// V7 body has `version = 7` recorded in `_schema_migrations`, so
/// `run_migrations` will never re-run V7's body again, no matter how it is
/// edited later. Round 2 (9b829cf4) edited `007-notes-seq.sql` in place to
/// add a backfill -- but on a database that already applied the original
/// V7, that edited body never executes, and round 2's *lazy* bootstrap
/// backfill (`notes-ddl.sql`) was itself gated on `notes_seq` being
/// globally empty. The moment exactly one note lands a `notes_seq` row
/// through the ordinary write path, that guard sees a non-empty table and
/// skips -- permanently stranding every older, still-unmapped note.
///
/// This builds exactly that ledger directly in SQLite: apply V1..V6, then
/// the original (backfill-less) V7 body, recording `version = 7`; insert
/// three pre-existing notes with no `notes_seq` row; insert one
/// post-upgrade note WITH a manually assigned `notes_seq` row (partial
/// population); and advance `sqlite_sequence` for `notes_seq` to a value
/// far above any row actually present (a stale high-water mark, as if
/// earlier rows had since been deleted). Reopening through the current
/// code (V8's forward repair migration plus the fixed anti-join lazy
/// bootstrap) must recover all three older notes in the very first probe.
#[tokio::test]
async fn probe_repairs_partial_notes_seq_left_by_original_v7_on_reopen() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("v7_partial_upgrade.db");
    let actor = "lambda:leo";
    let counterpart = "lambda:khive";

    let old_ids = [Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
    let post_upgrade_id = Uuid::new_v4();

    // The exact original V7 body (khive#827 head 87c25939): creates
    // `notes_seq` but performs no backfill at all.
    const V7_ORIGINAL_NO_BACKFILL: &str = "\
        CREATE TABLE IF NOT EXISTS notes_seq ( \
            seq     INTEGER PRIMARY KEY AUTOINCREMENT, \
            note_id TEXT NOT NULL UNIQUE \
        ); \
        CREATE INDEX IF NOT EXISTS idx_notes_seq_note_id ON notes_seq(note_id);";

    {
        let backend = khive_db::StorageBackend::sqlite(&path).expect("open backend");
        let writer = backend.pool().try_writer().expect("writer");
        let conn = writer.conn();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS _schema_migrations ( \
                 version INTEGER PRIMARY KEY, name TEXT NOT NULL, applied_at INTEGER NOT NULL);",
        )
        .expect("create migration tracking table");

        let now = chrono::Utc::now().timestamp_micros();
        for migration in khive_db::migrations::MIGRATIONS
            .iter()
            .filter(|m| m.version <= 6)
        {
            conn.execute_batch(migration.up)
                .unwrap_or_else(|e| panic!("apply V{}: {e}", migration.version));
            conn.execute(
                "INSERT INTO _schema_migrations (version, name, applied_at) VALUES (?1, ?2, ?3)",
                rusqlite::params![migration.version, migration.name, now],
            )
            .unwrap_or_else(|e| panic!("record V{}: {e}", migration.version));
        }

        // Apply the ORIGINAL (backfill-less) V7 body and record it as
        // applied, simulating a database that upgraded before round 2.
        conn.execute_batch(V7_ORIGINAL_NO_BACKFILL)
            .expect("apply original backfill-less V7");
        conn.execute(
            "INSERT INTO _schema_migrations (version, name, applied_at) VALUES (7, 'notes_seq', ?1)",
            rusqlite::params![now],
        )
        .expect("record original V7 as applied");

        // Three pre-existing (older) notes, inserted with no notes_seq row --
        // exactly what the original V7 leaves behind.
        for (i, id) in old_ids.iter().enumerate() {
            conn.execute(
                "INSERT INTO notes \
                 (id, namespace, kind, status, name, content, salience, decay_factor, expires_at, \
                  properties, created_at, updated_at, deleted_at) \
                 VALUES (?1, 'local', 'message', 'active', NULL, ?2, \
                         NULL, NULL, NULL, ?3, ?4, ?4, NULL)",
                rusqlite::params![
                    id.to_string(),
                    format!("pre-existing note {i}"),
                    json!({
                        "direction": "inbound",
                        "to_actor": actor,
                        "from_actor": counterpart,
                        "read": false,
                    })
                    .to_string(),
                    1_000_000_i64 + i as i64,
                ],
            )
            .unwrap_or_else(|e| panic!("insert pre-existing note {i}: {e}"));
        }

        // One post-upgrade note, inserted the normal way -- WITH a
        // notes_seq row, simulating `assign_note_seq` having run for it.
        conn.execute(
            "INSERT INTO notes \
             (id, namespace, kind, status, name, content, salience, decay_factor, expires_at, \
              properties, created_at, updated_at, deleted_at) \
             VALUES (?1, 'local', 'message', 'active', NULL, 'post-upgrade note', \
                     NULL, NULL, NULL, ?2, ?3, ?3, NULL)",
            rusqlite::params![
                post_upgrade_id.to_string(),
                json!({
                    "direction": "inbound",
                    "to_actor": actor,
                    "from_actor": counterpart,
                    "read": false,
                })
                .to_string(),
                2_000_000_i64,
            ],
        )
        .expect("insert post-upgrade note");
        conn.execute(
            "INSERT INTO notes_seq (note_id) VALUES (?1)",
            rusqlite::params![post_upgrade_id.to_string()],
        )
        .expect("assign notes_seq row to post-upgrade note");

        // Advance the AUTOINCREMENT high-water mark far past the one row
        // actually present, simulating earlier notes_seq rows having since
        // been deleted (e.g. a hard-deleted note). The repair must still
        // work correctly against a ledger whose next-assigned seq values
        // are nowhere near contiguous with created_at order.
        conn.execute(
            "UPDATE sqlite_sequence SET seq = 500 WHERE name = 'notes_seq'",
            [],
        )
        .expect("advance stale notes_seq high-water mark");
    }

    // Reopen through the normal runtime boot path -- this runs
    // `run_migrations` to latest (including V8's forward repair) and the
    // fixed anti-join lazy bootstrap.
    let config = RuntimeConfig {
        db_path: Some(path.clone()),
        default_namespace: Namespace::local(),
        embedding_model: None,
        additional_embedding_models: vec![],
        gate: Arc::new(AllowAllGate),
        packs: vec!["kg".to_string(), "comm".to_string()],
        backend_id: BackendId::main(),
        brain_profile: None,
        visible_namespaces: vec![],
        allowed_outbound_namespaces: vec![],
        actor_id: None,
    };
    let runtime = KhiveRuntime::new(config).expect("runtime reopens and migrates to latest");

    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(CommPack::new(runtime.clone()));
    let registry = builder.build().expect("registry builds");
    registry.apply_schema_plans(runtime.backend());

    let result = registry
        .dispatch("comm.probe", json!({ "actor": actor }))
        .await
        .expect("probe succeeds after partial-ledger v7->v8 upgrade");

    let messages = result["new_messages"].as_array().expect("array");
    let returned_ids: Vec<String> = messages
        .iter()
        .map(|m| m["id"].as_str().unwrap().to_string())
        .collect();

    for id in &old_ids {
        assert!(
            returned_ids.contains(&id.to_string()),
            "pre-existing note {id} left unmapped by the original V7 must be repaired \
             and appear in the first probe after upgrade: {returned_ids:?}"
        );
    }
    assert!(
        returned_ids.contains(&post_upgrade_id.to_string()),
        "the already-mapped post-upgrade note must still appear: {returned_ids:?}"
    );
    assert_eq!(
        messages.len(),
        4,
        "all three older notes plus the post-upgrade note must be visible: {messages:?}"
    );
}

/// Regression for #827 round-4 perf finding: the notes_seq anti-join repair
/// (`stores/note.rs::repair_notes_seq`, the same statement as V8's forward
/// migration) used to run its full `notes` table scan plus a temp B-tree for
/// the `ORDER BY` on *every* `notes_for_namespace` call -- on a large,
/// already-repaired ledger that serialized every caller behind the writer
/// mutex for a scan that could never find anything to repair. It must now
/// run at most once per backend (`StorageBackend`) for the process's
/// lifetime, gated by an atomic counter, not once per store acquisition.
///
/// Verified via `StorageBackend::notes_seq_repair_run_count` -- an actual
/// count of how many times the repair statement executed -- not via timing,
/// per the fix's own requirement that this be observable deterministically.
#[tokio::test]
async fn notes_seq_repair_runs_once_per_backend_not_per_store_acquisition() {
    let (_registry, runtime) = build_registry();
    let token = runtime
        .authorize(Namespace::local())
        .expect("authorize local namespace");

    // `build_registry()` already acquired a NoteStore once (to create the
    // `notes` table before applying schema plans), and `KhiveRuntime::new`/
    // `memory()` already ran `run_migrations` (including V8's forward
    // repair) before that -- so by this point the ledger is fully repaired
    // and the lazy repair has run exactly once.
    assert_eq!(
        runtime.backend().notes_seq_repair_run_count(),
        1,
        "the first store acquisition on this backend must run the repair exactly once"
    );

    // Plant a genuine message so there is real traffic through the store on
    // each acquisition below, then repeatedly re-acquire the NoteStore --
    // the exact shape of every real request dispatch through
    // `KhiveRuntime::notes`.
    let note_id = plant_inbound_message(
        &runtime,
        "lambda:leo",
        "lambda:khive",
        1_000_000,
        None,
        false,
    )
    .await;
    for _ in 0..5 {
        let store = runtime.notes(&token).expect("notes store");
        assert!(
            store.get_note(note_id).await.expect("get_note").is_some(),
            "the store returned on each acquisition must still be fully functional"
        );
    }

    assert_eq!(
        runtime.backend().notes_seq_repair_run_count(),
        1,
        "repeated store acquisition on an already-repaired ledger must not \
         re-run the notes_seq anti-join repair"
    );
}
