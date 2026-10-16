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

    let runtime = support::memory_runtime();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(khive_pack_comm::CommPack::new(runtime.clone()));
    builder.register(SchedulePack::new(runtime.clone()));
    let registry = builder.build().expect("registry builds");

    let tok = runtime.authorize(khive_types::Namespace::local()).unwrap();
    let note_store = runtime.notes(&tok).expect("note store accessible");

    // Step 1: insert the valid event FIRST (oldest created_at).
    // Using the verb path here would assign Utc::now() as created_at, but we
    // need to control ordering precisely, so use the storage layer too.
    let valid_at = "2099-11-11T11:11:11Z";
    let valid_note = Note {
        version: 1,
        key: None,
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
            version: 1,
            key: None,
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
