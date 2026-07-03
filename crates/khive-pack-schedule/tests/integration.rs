//! Smoke tests for the schedule pack (ADR-040).

use khive_pack_schedule::SchedulePack;
use khive_runtime::{KhiveRuntime, VerbRegistry, VerbRegistryBuilder};
use khive_types::Pack;

fn build_registry() -> (VerbRegistry, KhiveRuntime) {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(SchedulePack::new(runtime.clone()));
    let registry = builder.build().expect("registry builds");
    (registry, runtime)
}

#[test]
fn schedule_pack_declares_scheduled_event_note_kind() {
    assert!(SchedulePack::NOTE_KINDS.contains(&"scheduled_event"));
}

#[test]
fn schedule_pack_declares_four_handlers() {
    assert_eq!(SchedulePack::HANDLERS.len(), 4);
    let names: Vec<&str> = SchedulePack::HANDLERS.iter().map(|h| h.name).collect();
    assert!(names.contains(&"schedule.remind"));
    assert!(names.contains(&"schedule.schedule"));
    assert!(names.contains(&"schedule.agenda"));
    assert!(names.contains(&"schedule.cancel"));
}

#[test]
fn schedule_pack_requires_kg() {
    assert_eq!(SchedulePack::REQUIRES, &["kg"]);
}

#[tokio::test]
async fn remind_creates_pending_event() {
    let (registry, _rt) = build_registry();

    let result = registry
        .dispatch(
            "schedule.remind",
            serde_json::json!({
                "content": "check status",
                "at": "2099-06-01T09:00:00Z"
            }),
        )
        .await
        .expect("remind succeeds");

    assert!(result.get("id").is_some(), "remind returns id: {result}");
    assert_eq!(result["status"], "pending");
    assert_eq!(result["event_type"], "remind");
}

#[tokio::test]
async fn schedule_creates_pending_event_with_action() {
    let (registry, _rt) = build_registry();

    let result = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "create(kind=\"entity\", name=\"test\")",
                "at": "2099-06-01T10:00:00Z"
            }),
        )
        .await
        .expect("schedule succeeds");

    assert!(result.get("id").is_some(), "schedule returns id: {result}");
    assert_eq!(result["event_type"], "schedule");
}

#[tokio::test]
async fn agenda_returns_pending_events() {
    let (registry, _rt) = build_registry();

    registry
        .dispatch(
            "schedule.remind",
            serde_json::json!({ "content": "hello", "at": "2099-07-01T00:00:00Z" }),
        )
        .await
        .expect("remind succeeds");

    let agenda = registry
        .dispatch("schedule.agenda", serde_json::json!({ "limit": 10 }))
        .await
        .expect("agenda succeeds");

    let count = agenda["count"].as_u64().unwrap_or(0);
    assert!(
        count >= 1,
        "agenda should return at least 1 event: {agenda}"
    );
}

#[tokio::test]
async fn remind_with_invalid_repeat_is_rejected() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch(
            "schedule.remind",
            serde_json::json!({
                "content": "hello",
                "at": "2099-06-01T09:00:00Z",
                "repeat": "not-valid-cron"
            }),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("repeat") || err.to_string().contains("cron"));
}

#[tokio::test]
async fn test_full_id_returns_36_char_schedule() {
    let (registry, _rt) = build_registry();

    let result = registry
        .dispatch(
            "schedule.remind",
            serde_json::json!({ "content": "check status", "at": "2099-06-01T09:00:00Z" }),
        )
        .await
        .expect("remind succeeds");

    let id = result
        .get("id")
        .and_then(|v| v.as_str())
        .expect("id present");
    let full_id = result
        .get("full_id")
        .and_then(|v| v.as_str())
        .expect("full_id present");

    assert_eq!(id.len(), 8, "id must be 8-char short prefix");
    assert_eq!(full_id.len(), 36, "full_id must be 36-char hyphenated UUID");
    assert!(
        full_id.starts_with(id),
        "full_id must start with the short id prefix"
    );
    assert!(full_id.contains('-'), "full_id must be hyphenated format");
}

// ── S-C1 regression: RFC 3339 timestamp validation ──────────────────────────
//
// Invalid `at` values must be rejected before writing to storage.
// Valid RFC 3339 timestamps must succeed.

#[tokio::test]
async fn s_c1_schedule_valid_rfc3339_succeeds() {
    let (registry, _rt) = build_registry();

    let result = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "schedule.remind(content=\"test\", at=\"2099-12-31T00:00:00Z\")",
                "at": "2099-01-01T00:00:00Z"
            }),
        )
        .await
        .expect("schedule with valid RFC 3339 must succeed");

    assert_eq!(result["status"], "pending");
}

#[tokio::test]
async fn s_c1_schedule_invalid_at_not_a_date() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "schedule.remind(content=\"test\", at=\"2099-12-31T00:00:00Z\")",
                "at": "not-a-date"
            }),
        )
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("RFC 3339") || msg.contains("timestamp") || msg.contains("not-a-date"),
        "S-C1: error must reference RFC 3339 or the bad value; got: {msg}"
    );
}

#[tokio::test]
async fn s_c1_schedule_invalid_at_natural_language() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "schedule.remind(content=\"test\", at=\"2099-12-31T00:00:00Z\")",
                "at": "tomorrow at 3pm"
            }),
        )
        .await
        .unwrap_err();

    assert!(
        err.to_string().contains("RFC 3339") || err.to_string().contains("timestamp"),
        "S-C1: natural-language at must be rejected with RFC 3339 hint; got: {err}"
    );
}

#[tokio::test]
async fn s_c1_schedule_invalid_at_out_of_range_date() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "schedule.remind(content=\"test\", at=\"2099-12-31T00:00:00Z\")",
                "at": "2027-13-99"
            }),
        )
        .await
        .unwrap_err();

    assert!(
        err.to_string().contains("RFC 3339") || err.to_string().contains("timestamp"),
        "S-C1: out-of-range date must be rejected; got: {err}"
    );
}

#[tokio::test]
async fn s_c1_remind_invalid_at_is_rejected() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch(
            "schedule.remind",
            serde_json::json!({
                "content": "hello",
                "at": "invalid"
            }),
        )
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("RFC 3339") || msg.contains("timestamp") || msg.contains("invalid"),
        "S-C1: remind with invalid at must be rejected; got: {msg}"
    );
}

#[tokio::test]
async fn s_c1_remind_valid_rfc3339_succeeds() {
    let (registry, _rt) = build_registry();

    let result = registry
        .dispatch(
            "schedule.remind",
            serde_json::json!({
                "content": "morning standup",
                "at": "2099-06-15T09:00:00+00:00"
            }),
        )
        .await
        .expect("remind with valid RFC 3339 offset format must succeed");

    assert_eq!(result["status"], "pending");
}

#[tokio::test]
async fn s_c1_agenda_only_shows_valid_events() {
    // After at-validation is enforced, agenda() must not show corrupt events.
    // This test verifies the invariant: all events in agenda have parseable trigger_at.
    let (registry, _rt) = build_registry();

    registry
        .dispatch(
            "schedule.remind",
            serde_json::json!({ "content": "valid event", "at": "2099-01-01T10:00:00Z" }),
        )
        .await
        .expect("remind with valid at must succeed");

    let agenda = registry
        .dispatch("schedule.agenda", serde_json::json!({ "limit": 50 }))
        .await
        .expect("agenda must succeed");

    let events = agenda["events"].as_array().expect("events array");
    for event in events {
        let trigger_at = event["properties"]["trigger_at"]
            .as_str()
            .expect("trigger_at must be a string");
        assert!(
            trigger_at.parse::<chrono::DateTime<chrono::Utc>>().is_ok(),
            "S-C1/M-1: agenda event trigger_at {trigger_at:?} must be a valid RFC 3339 timestamp"
        );
    }
}

#[tokio::test]
async fn test_cancel_accepts_short_id() {
    let (registry, _rt) = build_registry();

    let reminded = registry
        .dispatch(
            "schedule.remind",
            serde_json::json!({ "content": "cancel me by short id", "at": "2099-07-01T12:00:00Z" }),
        )
        .await
        .expect("remind succeeds");

    let short = reminded
        .get("id")
        .and_then(|v| v.as_str())
        .expect("id present");
    let full_id = reminded
        .get("full_id")
        .and_then(|v| v.as_str())
        .expect("full_id present");
    assert_eq!(full_id.len(), 36, "full_id from remind must be 36-char");

    // Cancel using only the 8-char short prefix — must succeed.
    let result = registry
        .dispatch("schedule.cancel", serde_json::json!({ "id": short }))
        .await
        .expect("cancel with 8-char short id succeeds");

    assert_eq!(
        result.get("status").and_then(|v| v.as_str()),
        Some("cancelled"),
        "cancel returns status=cancelled — got {result}"
    );
    let cancel_full_id = result
        .get("full_id")
        .and_then(|v| v.as_str())
        .expect("cancel returns full_id");
    assert_eq!(
        cancel_full_id.len(),
        36,
        "cancel response full_id must be 36-char"
    );
    assert!(
        cancel_full_id.starts_with(short),
        "cancel response full_id starts with short prefix"
    );
}

// ── #544 regression: cancel already-cancelled event ─────────────────────────

#[tokio::test]
async fn cancel_rejects_already_cancelled_event() {
    let (registry, _rt) = build_registry();

    let reminded = registry
        .dispatch(
            "schedule.remind",
            serde_json::json!({
                "content": "cancel once",
                "at": "2099-07-01T12:00:00Z"
            }),
        )
        .await
        .expect("remind succeeds");
    let full_id = reminded["full_id"].as_str().expect("full_id present");

    let first = registry
        .dispatch("schedule.cancel", serde_json::json!({ "id": full_id }))
        .await
        .expect("first cancel succeeds");
    assert_eq!(
        first["status"].as_str(),
        Some("cancelled"),
        "first cancel must return status=cancelled"
    );

    let err = registry
        .dispatch("schedule.cancel", serde_json::json!({ "id": full_id }))
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("not pending") && msg.contains("cancelled"),
        "second cancel must report the event is no longer pending, got: {msg}"
    );
}

// ── C3 regression: past dates rejected ───────────────────────────────────────

#[tokio::test]
async fn c3_schedule_past_date_rejected() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "schedule.remind(content=\"past\", at=\"2099-12-31T00:00:00Z\")",
                "at": "2020-01-01T00:00:00Z"
            }),
        )
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("past") || msg.contains("future"),
        "C3: past at must be rejected with past/future hint; got: {msg}"
    );
}

#[tokio::test]
async fn c3_remind_past_date_rejected() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch(
            "schedule.remind",
            serde_json::json!({
                "content": "stale reminder",
                "at": "2019-06-01T09:00:00Z"
            }),
        )
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("past") || msg.contains("future"),
        "C3: past remind.at must be rejected; got: {msg}"
    );
}

#[tokio::test]
async fn c3_agenda_never_shows_past_pending_events() {
    // Since past dates are rejected at write time, agenda must never contain
    // pending events with trigger_at in the past.
    let (registry, _rt) = build_registry();

    // Insert one valid future event.
    registry
        .dispatch(
            "schedule.remind",
            serde_json::json!({ "content": "future check", "at": "2099-12-31T23:59:59Z" }),
        )
        .await
        .expect("future remind must succeed");

    let agenda = registry
        .dispatch("schedule.agenda", serde_json::json!({ "limit": 100 }))
        .await
        .expect("agenda must succeed");

    let now = chrono::Utc::now();
    let events = agenda["events"].as_array().expect("events array");
    for event in events {
        let trigger_at = event["properties"]["trigger_at"]
            .as_str()
            .expect("trigger_at must be present");
        let instant = trigger_at
            .parse::<chrono::DateTime<chrono::Utc>>()
            .expect("trigger_at must be parseable");
        assert!(
            instant > now,
            "C3: agenda must never contain past-pending events; found {trigger_at}"
        );
    }
}

// ── C4 regression: unparseable DSL action rejected ───────────────────────────

#[tokio::test]
async fn c4_schedule_bogus_action_rejected() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "bogus-not-a-valid-verb()",
                "at": "2099-01-01T00:00:00Z"
            }),
        )
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("DSL") || msg.contains("action") || msg.contains("invalid"),
        "C4: bogus action must be rejected with DSL error hint; got: {msg}"
    );
}

#[tokio::test]
async fn c4_schedule_single_char_action_rejected() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "x",
                "at": "2099-01-01T00:00:00Z"
            }),
        )
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("DSL") || msg.contains("action") || msg.contains("invalid"),
        "C4: single-char action must be rejected; got: {msg}"
    );
}

#[tokio::test]
async fn c4_schedule_valid_action_succeeds() {
    let (registry, _rt) = build_registry();

    // A well-formed verb call must be accepted.
    let result = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "schedule.remind(content=\"hello world\", at=\"2099-12-31T00:00:00Z\")",
                "at": "2099-06-01T10:00:00Z"
            }),
        )
        .await
        .expect("schedule with valid DSL action must succeed");

    assert_eq!(result["status"], "pending");
}

// ── H1 regression: agenda from/to uses parsed timestamps ─────────────────────

#[tokio::test]
async fn h1_agenda_from_filter_uses_parsed_timestamps() {
    let (registry, _rt) = build_registry();

    // Insert events at two different future times.
    registry
        .dispatch(
            "schedule.remind",
            serde_json::json!({ "content": "early", "at": "2099-01-01T10:00:00Z" }),
        )
        .await
        .expect("remind 1 succeeds");
    registry
        .dispatch(
            "schedule.remind",
            serde_json::json!({ "content": "late", "at": "2099-12-31T10:00:00Z" }),
        )
        .await
        .expect("remind 2 succeeds");

    // Only events at or after 2099-06-01 should be returned.
    let agenda = registry
        .dispatch(
            "schedule.agenda",
            serde_json::json!({ "from": "2099-06-01T00:00:00Z", "limit": 50 }),
        )
        .await
        .expect("agenda with from filter succeeds");

    let events = agenda["events"].as_array().expect("events array");
    // The early event (2099-01-01) must not appear.
    for event in events {
        let trigger_at = event["properties"]["trigger_at"]
            .as_str()
            .expect("trigger_at present");
        let instant = trigger_at
            .parse::<chrono::DateTime<chrono::Utc>>()
            .expect("parseable");
        let from = "2099-06-01T00:00:00Z"
            .parse::<chrono::DateTime<chrono::Utc>>()
            .unwrap();
        assert!(
            instant >= from,
            "H1: agenda.from filter must exclude events before the bound; found {trigger_at}"
        );
    }
}

#[tokio::test]
async fn h1_agenda_rejects_invalid_from() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch(
            "schedule.agenda",
            serde_json::json!({ "from": "not-a-date", "limit": 10 }),
        )
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("RFC 3339") || msg.contains("timestamp") || msg.contains("not-a-date"),
        "H1: invalid agenda.from must be rejected; got: {msg}"
    );
}

#[tokio::test]
async fn h1_agenda_rejects_invalid_to() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch(
            "schedule.agenda",
            serde_json::json!({ "to": "not-a-date", "limit": 10 }),
        )
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("RFC 3339") || msg.contains("timestamp") || msg.contains("not-a-date"),
        "H1: invalid agenda.to must be rejected; got: {msg}"
    );
}

// ── H5 regression: trigger_at preserves caller's original RFC3339 string ─────
//
// The fix: callers submit a timestamp with an offset; the pack must return
// the *original* string (preserving wall time + offset) rather than
// canonicalising to UTC.  The stored instant must still be correct for
// comparison purposes (validated separately through agenda ordering tests).

#[tokio::test]
async fn h5_schedule_at_with_offset_preserves_original_string() {
    let (registry, _rt) = build_registry();

    // Input has +02:00 offset. The canonical UTC equivalent would be
    // 2099-01-01T22:00:00Z — but the response must return the ORIGINAL string.
    let input_at = "2099-01-02T00:00:00+02:00";
    let result = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "schedule.remind(content=\"tz-test\", at=\"2099-12-31T00:00:00Z\")",
                "at": input_at
            }),
        )
        .await
        .expect("schedule with +02:00 offset must succeed");

    let trigger_at = result["trigger_at"].as_str().expect("trigger_at present");
    // Must preserve the caller's original string exactly.
    assert_eq!(
        trigger_at, input_at,
        "H5: trigger_at in response must preserve caller's original RFC3339 string; got {trigger_at}"
    );
    // The original string must still be parseable as a valid RFC3339 timestamp.
    assert!(
        trigger_at.parse::<chrono::DateTime<chrono::Utc>>().is_ok(),
        "H5: stored trigger_at must be a valid RFC 3339 timestamp; got {trigger_at}"
    );
}

#[tokio::test]
async fn h5_remind_at_with_offset_preserves_original_string() {
    let (registry, _rt) = build_registry();

    let input_at = "2099-06-15T09:00:00+05:30";
    let result = registry
        .dispatch(
            "schedule.remind",
            serde_json::json!({
                "content": "tz-remind",
                "at": input_at
            }),
        )
        .await
        .expect("remind with +05:30 offset must succeed");

    let trigger_at = result["trigger_at"].as_str().expect("trigger_at present");
    // Must preserve the caller's original string exactly.
    assert_eq!(
        trigger_at, input_at,
        "H5: trigger_at in remind response must preserve caller's original RFC3339 string; got {trigger_at}"
    );
    // The original string must still be parseable.
    assert!(
        trigger_at.parse::<chrono::DateTime<chrono::Utc>>().is_ok(),
        "H5: stored trigger_at must be a valid RFC 3339 timestamp; got {trigger_at}"
    );
}

#[tokio::test]
async fn h5_utc_input_preserved_as_is() {
    let (registry, _rt) = build_registry();

    // UTC input (Z suffix) must also be returned as-is.
    let input_at = "2099-03-10T12:00:00Z";
    let result = registry
        .dispatch(
            "schedule.remind",
            serde_json::json!({ "content": "utc-tz-test", "at": input_at }),
        )
        .await
        .expect("remind with Z suffix must succeed");

    let trigger_at = result["trigger_at"].as_str().expect("trigger_at present");
    assert_eq!(
        trigger_at, input_at,
        "H5: UTC input must be returned unchanged; got {trigger_at}"
    );
}

// ── H2 regression: agenda paginates past corrupt legacy rows ─────────────────
//
// Inserts a valid pending event FIRST (oldest created_at), then inserts more
// than one page worth of corrupt legacy rows AFTER it (newer created_at). The
// runtime list_notes path orders by created_at DESC, so the corrupt rows are
// returned first — the valid event sits below the first page.
//
// Without the paginating loop fix, `agenda()` would only see the first page of
// corrupt rows and return zero events, never finding the valid one beneath.
// With the fix, the loop crosses the page boundary and surfaces the valid
// event.

#[tokio::test]
async fn h2_agenda_finds_valid_event_past_corrupt_legacy_rows() {
    use chrono::Utc;
    use khive_storage::Note;
    use serde_json::json;

    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(SchedulePack::new(runtime.clone()));
    let registry = builder.build().expect("registry builds");

    let tok = runtime.authorize(khive_types::Namespace::local()).unwrap();
    let note_store = runtime.notes(&tok).expect("note store accessible");

    // Step 1: insert the valid event FIRST (oldest created_at).
    // Using the verb path here would assign Utc::now() as created_at, but we
    // need to control ordering precisely, so use the storage layer too.
    let valid_at = "2099-11-11T11:11:11Z";
    let valid_note = Note {
        id: uuid::Uuid::new_v4(),
        namespace: "local".to_string(),
        kind: "scheduled_event".to_string(),
        status: "active".to_string(),
        name: None,
        content: "valid-event".to_string(),
        salience: None,
        decay_factor: None,
        expires_at: None,
        properties: Some(json!({
            "trigger_at": valid_at,
            "status": "pending",
            "event_type": "remind",
            "payload": null,
            "fired_at": null,
            "cancelled_at": null,
        })),
        // Anchor created_at well in the past so all corrupt rows have newer ts.
        created_at: 1_700_000_000_000_000_i64,
        updated_at: Utc::now().timestamp_micros(),
        deleted_at: None,
    };
    note_store
        .upsert_note(valid_note)
        .await
        .expect("valid note inserted");

    // Step 2: insert > PAGE_SIZE (200) corrupt rows AFTER the valid event so
    // newest-first pagination returns them before the valid row. The handler
    // const PAGE_SIZE = 200; we use 250 so the valid event is on page 2.
    let now_micros = Utc::now().timestamp_micros();
    for i in 0..250u32 {
        let corrupt = Note {
            id: uuid::Uuid::new_v4(),
            namespace: "local".to_string(),
            kind: "scheduled_event".to_string(),
            status: "active".to_string(),
            name: None,
            content: format!("corrupt-legacy-{i}"),
            salience: None,
            decay_factor: None,
            expires_at: None,
            properties: Some(json!({
                "trigger_at": "not-a-date",
                "status": "pending",
                "event_type": "remind",
                "payload": null,
                "fired_at": null,
                "cancelled_at": null,
            })),
            // Newer than the valid event, increasing so each row is distinct.
            created_at: now_micros + (i as i64 * 1000),
            updated_at: now_micros,
            deleted_at: None,
        };
        note_store
            .upsert_note(corrupt)
            .await
            .expect("corrupt note inserted");
    }

    // agenda() must return the valid event despite corrupt rows preceding it.
    let agenda = registry
        .dispatch("schedule.agenda", serde_json::json!({ "limit": 10 }))
        .await
        .expect("agenda must succeed");

    let events = agenda["events"].as_array().expect("events array");
    assert!(
        !events.is_empty(),
        "H2: agenda must return at least one event; corrupt legacy rows must not hide valid ones"
    );

    // Every returned event must have a parseable trigger_at.
    for event in events {
        let trigger_at = event["properties"]["trigger_at"]
            .as_str()
            .expect("trigger_at present");
        assert!(
            trigger_at.parse::<chrono::DateTime<chrono::Utc>>().is_ok(),
            "H2: every agenda event must have a valid RFC 3339 trigger_at; got {trigger_at:?}"
        );
    }

    // The valid event must be present.
    let found = events
        .iter()
        .any(|e| e["properties"]["trigger_at"].as_str() == Some(valid_at));
    assert!(
        found,
        "H2: valid-event with trigger_at={valid_at:?} must appear in agenda; got: {events:?}"
    );
}

// ── schema_plan regression: SchedulePack declares ADR-040 trigger index ───────

#[tokio::test]
async fn schedule_pack_exposes_non_empty_schema_plan() {
    use khive_runtime::PackRuntime;
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let pack = SchedulePack::new(runtime);
    let plan = pack.schema_plan();

    assert!(
        !plan.is_empty(),
        "SchedulePack must return a non-empty SchemaPlan (ADR-040 §283)"
    );
    assert_eq!(plan.pack, "schedule", "SchemaPlan.pack must be 'schedule'");
    assert!(
        !plan.statements.is_empty(),
        "schema plan must have at least one DDL statement"
    );

    let combined = plan.statements.join(" ");
    assert!(
        combined.contains("idx_schedule_trigger"),
        "schema plan must declare idx_schedule_trigger index; got: {combined}"
    );
    assert!(
        combined.contains("CREATE INDEX IF NOT EXISTS"),
        "schema plan DDL must be idempotent (CREATE INDEX IF NOT EXISTS); got: {combined}"
    );
    // Index now uses WHERE deleted_at IS NULL so the parameterized kind = ?N
    // predicate can use the index (literal WHERE kind = 'scheduled_event' blocks this).
    assert!(
        combined.contains("deleted_at IS NULL"),
        "schema plan index must use WHERE deleted_at IS NULL partial condition; got: {combined}"
    );
}

#[tokio::test]
async fn verb_registry_aggregates_schedule_schema_plan() {
    let (registry, _rt) = build_registry();
    let plans = registry.all_schema_plans();
    assert!(
        plans.iter().any(|p| p.pack == "schedule"),
        "registry must expose schedule schema plan; got packs: {:?}",
        plans.iter().map(|p| p.pack).collect::<Vec<_>>()
    );
    let sched_plan = plans
        .iter()
        .find(|p| p.pack == "schedule")
        .expect("schedule plan present");
    assert!(
        !sched_plan.is_empty(),
        "schedule schema plan must have DDL statements"
    );
}

// ── SCH-AUD-002 regression: malformed five-field cron rejected ───────────────

#[tokio::test]
async fn sch_aud_002_malformed_five_field_cron_rejected() {
    let (registry, _rt) = build_registry();

    // Five fields but all garbage — must be rejected.
    let err = registry
        .dispatch(
            "schedule.remind",
            serde_json::json!({
                "content": "bad cron",
                "at": "2099-06-01T09:00:00Z",
                "repeat": "foo bar baz qux zap"
            }),
        )
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("repeat") || msg.contains("cron") || msg.contains("invalid"),
        "SCH-AUD-002: malformed five-field cron must be rejected; got: {msg}"
    );
}

#[tokio::test]
async fn sch_aud_002_out_of_range_cron_minute_rejected() {
    let (registry, _rt) = build_registry();

    // Minute field 99 is out of range (0–59).
    let err = registry
        .dispatch(
            "schedule.remind",
            serde_json::json!({
                "content": "bad minute",
                "at": "2099-06-01T09:00:00Z",
                "repeat": "99 * * * *"
            }),
        )
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("minute") || msg.contains("range") || msg.contains("99"),
        "SCH-AUD-002: out-of-range minute field must be rejected; got: {msg}"
    );
}

#[tokio::test]
async fn sch_aud_002_valid_wildcard_cron_accepted() {
    let (registry, _rt) = build_registry();

    // All-wildcard five-field cron must be accepted.
    let result = registry
        .dispatch(
            "schedule.remind",
            serde_json::json!({
                "content": "wildcard cron",
                "at": "2099-06-01T09:00:00Z",
                "repeat": "* * * * *"
            }),
        )
        .await
        .expect("SCH-AUD-002: all-wildcard cron must be accepted");
    assert_eq!(result["status"], "pending");
}

#[tokio::test]
async fn sch_aud_002_valid_numeric_cron_accepted() {
    let (registry, _rt) = build_registry();

    // 0 9 * * 1 (every Monday at 09:00) — standard five-field cron.
    let result = registry
        .dispatch(
            "schedule.remind",
            serde_json::json!({
                "content": "monday morning",
                "at": "2099-06-01T09:00:00Z",
                "repeat": "0 9 * * 1"
            }),
        )
        .await
        .expect("SCH-AUD-002: valid numeric cron must be accepted");
    assert_eq!(result["status"], "pending");
}

// ── SCH-AUD-003 regression: agenda limit=0 and limit>200 rejected ────────────

#[tokio::test]
async fn sch_aud_003_agenda_limit_zero_rejected() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch("schedule.agenda", serde_json::json!({ "limit": 0 }))
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("limit") || msg.contains("range") || msg.contains('0'),
        "SCH-AUD-003: limit=0 must be rejected; got: {msg}"
    );
}

#[tokio::test]
async fn sch_aud_003_agenda_limit_over_max_rejected() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch("schedule.agenda", serde_json::json!({ "limit": 201 }))
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("limit") || msg.contains("range") || msg.contains("201"),
        "SCH-AUD-003: limit=201 must be rejected; got: {msg}"
    );
}

#[tokio::test]
async fn sch_aud_003_agenda_limit_boundary_values_accepted() {
    let (registry, _rt) = build_registry();

    // limit=1 and limit=200 are valid boundary values.
    for limit in [1u32, 200u32] {
        registry
            .dispatch("schedule.agenda", serde_json::json!({ "limit": limit }))
            .await
            .unwrap_or_else(|e| panic!("SCH-AUD-003: limit={limit} must be accepted; got: {e}"));
    }
}

// ── SCH-AUD-001 regression: cancel non-object properties returns error ────────

#[tokio::test]
async fn sch_aud_001_cancel_with_string_properties_returns_error() {
    use khive_runtime::KhiveRuntime;
    use khive_storage::Note;

    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = khive_runtime::VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(SchedulePack::new(runtime.clone()));
    let registry = builder.build().expect("registry builds");

    let tok = runtime.authorize(khive_types::Namespace::local()).unwrap();
    let note_store = runtime.notes(&tok).expect("note store accessible");

    // Insert a scheduled_event with string (non-object) properties — simulates
    // a corrupt row that would previously cause a panic in handle_cancel.
    let corrupt_id = uuid::Uuid::new_v4();
    let corrupt = Note {
        id: corrupt_id,
        namespace: "local".to_string(),
        kind: "scheduled_event".to_string(),
        status: "active".to_string(),
        name: None,
        content: "corrupt-props".to_string(),
        salience: None,
        decay_factor: None,
        expires_at: None,
        // Properties is a string instead of an object.
        properties: Some(serde_json::json!("not-an-object")),
        created_at: chrono::Utc::now().timestamp_micros(),
        updated_at: chrono::Utc::now().timestamp_micros(),
        deleted_at: None,
    };
    note_store
        .upsert_note(corrupt)
        .await
        .expect("corrupt note inserted");

    // cancel must return an error, NOT panic.
    let err = registry
        .dispatch(
            "schedule.cancel",
            serde_json::json!({ "id": corrupt_id.to_string() }),
        )
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("malformed") || msg.contains("properties") || msg.contains("object"),
        "SCH-AUD-001: cancel with non-object properties must return an error, not panic; got: {msg}"
    );
}
