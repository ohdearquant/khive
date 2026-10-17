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

/// #110, World 2 (diagnosed, not incidental): the outer `at` and a nested
/// action's own `at` are genuinely different timestamps that can diverge —
/// outer = when `schedule.schedule` fires and dispatches the stored `action`;
/// inner = whatever the nested verb's own `at` means (here, the reminder's
/// own future trigger time). Round-trip both through `schedule.agenda` and
/// assert they land in distinct places: the outer `at` becomes this event's
/// own `trigger_at`, while the inner `at` survives untouched, literally,
/// inside the stored `action` payload — nothing coerces or overwrites either
/// with the other.
#[tokio::test]
async fn s_c1_schedule_outer_and_nested_at_are_independent_and_both_preserved() {
    let (registry, _rt) = build_registry();

    let outer_at = "2099-01-01T00:00:00Z";
    let inner_at = "2099-12-31T00:00:00Z";
    assert_ne!(outer_at, inner_at, "the two `at`s must genuinely differ");

    registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": format!("schedule.remind(content=\"renew\", at=\"{inner_at}\")"),
                "at": outer_at,
            }),
        )
        .await
        .expect("schedule with distinct outer/inner at must succeed");

    let agenda = registry
        .dispatch("schedule.agenda", serde_json::json!({}))
        .await
        .expect("agenda must succeed");
    let events = agenda["events"]
        .as_array()
        .expect("agenda.events is an array");
    let event = events
        .iter()
        .find(|e| e["properties"]["event_type"] == "schedule")
        .expect("the scheduled event must be on the agenda");

    assert_eq!(
        event["properties"]["trigger_at"], outer_at,
        "the OUTER at must be this event's own trigger_at (when it fires)"
    );
    let payload = event["properties"]["payload"]
        .as_str()
        .expect("payload must be the stored action string");
    assert!(
        payload.contains(inner_at),
        "the INNER at must survive verbatim inside the stored action payload, \
         independent of the outer trigger_at; payload was: {payload}"
    );
    assert_ne!(
        event["properties"]["trigger_at"].as_str().unwrap(),
        inner_at,
        "the outer trigger_at must NOT have been overwritten by the nested at"
    );
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
