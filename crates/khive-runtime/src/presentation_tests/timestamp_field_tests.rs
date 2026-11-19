#[test]
fn agent_renders_old_timestamp_exactly() {
    let v = json!({"created_at": "2020-01-01T10:30:45.123456Z"});
    let out = agent(v);
    assert_eq!(out["created_at"], json!("2020-01-01T10:30:45.123456Z"));
    assert!(out.get("created_at_relative").is_none());
}

#[test]
fn agent_renders_recent_timestamp_exactly() {
    let ts_unix = NOW - 180;
    let ts = unix_to_iso8601(ts_unix);
    let v = json!({"updated_at": ts.clone()});
    let out = agent(v);
    assert_eq!(out["updated_at"], json!(ts));
}

#[test]
fn agent_does_not_compact_top_level_trigger_at_field() {
    // Regression for #871: `schedule.remind`'s create response returns
    // `trigger_at` as a top-level convenience field (sibling to `id`,
    // `full_id`), not nested under `"properties"`. The pre-existing
    // `inside_properties` guard alone did not protect it, so Agent-mode
    // compaction rewrote it: `at` here is far outside the 24h relative
    // window, so pre-fix it would have been minute-truncated to
    // "2026-07-11T19:00", discarding the seconds and the "-04:00"
    // offset the caller needs to round-trip the exact value verbatim.
    let at = "2026-07-11T19:00:00-04:00";
    let v = json!({
        "id": "a1b2c3d4",
        "full_id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
        "event_type": "remind",
        "trigger_at": at,
        "repeat": null,
        "status": "pending",
    });
    let out = agent(v);
    assert_eq!(out["trigger_at"], json!(at));
}

#[test]
fn agent_does_not_compact_top_level_trigger_at_utc() {
    let at = "2026-07-11T23:00:00Z";
    let v = json!({"trigger_at": at});
    let out = agent(v);
    assert_eq!(out["trigger_at"], json!(at));
}

#[test]
fn agent_does_not_compact_top_level_trigger_at_offset_less() {
    let at = "2026-07-11T23:00:00";
    let v = json!({"trigger_at": at});
    let out = agent(v);
    assert_eq!(out["trigger_at"], json!(at));
}

#[test]
fn agent_still_renders_other_top_level_timestamps_alongside_trigger_at() {
    // The `trigger_at` exemption is scoped to that field name only — a
    // sibling generic timestamp field still converts to exact UTC.
    let v = json!({
        "trigger_at": "2026-07-11T19:00:00-04:00",
        "created_at": "2020-01-01T10:30:45.123456Z",
    });
    let out = agent(v);
    assert_eq!(out["trigger_at"], json!("2026-07-11T19:00:00-04:00"));
    assert_eq!(out["created_at"], json!("2020-01-01T10:30:45.123456Z"));
}

#[test]
fn agent_does_not_compact_top_level_due() {
    // gtd.assign/gtd.tasks/gtd.next return the caller-supplied `due` as
    // a top-level convenience field mirroring `properties.due`; it must
    // round-trip verbatim through Agent-mode presentation, the same
    // guarantee already given to `trigger_at`.
    let due = "2026-08-01T09:30:15-04:00";
    let v = json!({"due": due});
    let out = agent(v);
    assert_eq!(out["due"], json!(due));
}

#[test]
fn agent_still_protects_nested_trigger_at_under_properties() {
    // Pre-existing protection (agenda/get responses nest trigger_at
    // under "properties") must remain intact alongside the new
    // top-level, field-name-based guard.
    let at = "2026-07-11T19:00:00-04:00";
    let v = json!({
        "id": "a1b2c3d4",
        "properties": {"trigger_at": at, "status": "pending"},
    });
    let out = agent(v);
    assert_eq!(out["properties"]["trigger_at"], json!(at));
}
