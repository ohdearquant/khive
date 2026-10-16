use super::*;
use serde_json::json;

/// A fixed "now" for deterministic tests: 2025-05-23T16:08:00Z.
const NOW: i64 = 1_748_016_480;

include!("presentation_tests/timestamp_conversion_tests.rs");

fn agent(v: Value) -> Value {
    present(v, PresentationMode::Agent, NOW)
}

include!("presentation_tests/agent_properties_and_empty_value_tests.rs");

include!("presentation_tests/heartbeat_null_and_ranking_display_tests.rs");

include!("presentation_tests/timestamp_field_tests.rs");

#[test]
fn agent_recurses_into_nested_objects() {
    let v = json!({
        "items": [
            {
                "id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
                "tags": [],
                "score": 0.9999
            }
        ]
    });
    let out = agent(v);
    let item = &out["items"][0];
    assert_eq!(item["id"], json!("a1b2c3d4"));
    assert!(item.get("tags").is_none());
    let s = item["score"].as_f64().unwrap();
    assert!((s - 1.0).abs() < 1e-9);
}

// full_id must never be shortened in Agent mode: it's the caller's
// stable chaining handle.
#[test]
fn agent_preserves_full_id_as_36_chars() {
    let uuid = "a1b2c3d4-e5f6-7890-abcd-ef1234567890";
    let v = json!({"id": uuid, "full_id": uuid, "title": "X"});
    let out = agent(v);
    // `id` is shortened to 8 chars
    assert_eq!(
        out["id"],
        json!("a1b2c3d4"),
        "id should be 8-char short form"
    );
    // `full_id` must remain the full 36-char UUID
    assert_eq!(
        out["full_id"].as_str().unwrap().len(),
        36,
        "full_id must be 36 chars in agent mode"
    );
    assert_eq!(
        out["full_id"],
        json!(uuid),
        "full_id must equal the original UUID"
    );
    // Verify the invariant: full_id starts with the short id prefix
    assert!(
        out["full_id"]
            .as_str()
            .unwrap()
            .starts_with(out["id"].as_str().unwrap()),
        "full_id must start with the short id prefix"
    );
}

#[test]
fn is_canonical_uuid_recognizes_valid() {
    assert!(is_canonical_uuid("a1b2c3d4-e5f6-7890-abcd-ef1234567890"));
    assert!(!is_canonical_uuid("a1b2c3d4"));
    assert!(!is_canonical_uuid("not-a-uuid-at-all-here---------"));
}

#[test]
fn looks_like_iso8601_recognizes_valid() {
    assert!(looks_like_iso8601("2026-05-23T16:18:15.234567Z"));
    assert!(!looks_like_iso8601("not a timestamp"));
    assert!(!looks_like_iso8601("2026-05-23"));
}

/// Format Unix seconds as ISO-8601 for test construction.
fn unix_to_iso8601(unix: i64) -> String {
    let (y, mo, d, h, mi, s) = unix_to_civil(unix);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

fn unix_to_civil(unix: i64) -> (i64, i64, i64, i64, i64, i64) {
    let s = unix % 86400;
    let days = unix / 86400;
    let h = s / 3600;
    let m = (s % 3600) / 60;
    let sec = s % 60;
    // Howard Hinnant civil_from_days
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    (y, mo, d, h, m, sec)
}

#[test]
fn agent_does_not_shorten_uuid_shaped_content_fields() {
    let uuid = "a1b2c3d4-e5f6-7890-abcd-ef1234567890";
    let out = agent(json!({
        "id": uuid,
        "full_id": uuid,
        "content": uuid,
        "description": uuid,
        "title": uuid,
        "query": uuid,
    }));

    assert_eq!(out["id"], json!("a1b2c3d4"));
    assert_eq!(out["full_id"], json!(uuid));
    assert_eq!(out["content"], json!(uuid));
    assert_eq!(out["description"], json!(uuid));
    assert_eq!(out["title"], json!(uuid));
    assert_eq!(out["query"], json!(uuid));
}

#[test]
fn agent_shortens_suffix_id_fields() {
    let uuid = "a1b2c3d4-e5f6-7890-abcd-ef1234567890";
    let out = agent(json!({
        "note_id": uuid,
        "source_id": uuid,
        "target_id": uuid,
    }));

    assert_eq!(out["note_id"], json!("a1b2c3d4"));
    assert_eq!(out["source_id"], json!("a1b2c3d4"));
    assert_eq!(out["target_id"], json!("a1b2c3d4"));
}

#[test]
fn agent_preserves_strict_round_trip_uuid_fields() {
    let uuid = "a1b2c3d4-e5f6-7890-abcd-ef1234567890";
    let out = agent(json!({
        "context_entity_id": uuid,
        "thread_id": uuid,
        "session_id": uuid,
        "project_id": uuid,
        "properties": {
            "context_entity_id": uuid,
            "thread_id": uuid,
            "outbound_ref": uuid,
            "parent_id": uuid,
            "session_id": uuid,
            "project_id": uuid,
        },
        "parent_id": uuid,
    }));

    assert_eq!(out["context_entity_id"], json!(uuid));
    assert_eq!(out["thread_id"], json!(uuid));
    assert_eq!(out["session_id"], json!(uuid));
    assert_eq!(out["project_id"], json!(uuid));
    assert_eq!(out["properties"]["context_entity_id"], json!(uuid));
    assert_eq!(out["properties"]["thread_id"], json!(uuid));
    assert_eq!(out["properties"]["outbound_ref"], json!(uuid));
    assert_eq!(out["properties"]["parent_id"], json!(uuid));
    assert_eq!(out["properties"]["session_id"], json!(uuid));
    assert_eq!(out["properties"]["project_id"], json!(uuid));
    assert_eq!(out["parent_id"], json!(uuid));
}

include!("presentation_tests/json_output_tests.rs");

include!("presentation_tests/auto_table_display_tests.rs");

#[test]
fn redundancy_drop_no_longer_hoists_outside_tables() {
    // Rule-separating control for the table-scoped hoist: the §7 pre-pass
    // alone must leave the properties bag in place, so a single record's
    // compact-JSON fallback keeps its shape.
    let v = json!({
        "id": "abc",
        "properties": {
            "trigger_at": "2026-09-01T14:00:00-04:00",
            "status": "pending"
        }
    });
    let reduced = apply_redundancy_drop(v, RedundancyScope::View);
    assert!(
        reduced.get("trigger_at").is_none() && reduced.get("status").is_none(),
        "the pre-pass must not hoist; the hoist is table-path only"
    );
    let props = reduced.get("properties").expect("properties must remain");
    assert_eq!(
        props.get("status").and_then(Value::as_str),
        Some("pending"),
        "fallback shape keeps payload fields inside properties"
    );
}

#[test]
fn sibling_string_with_newline_renders_as_json_literal() {
    // Escaping contract: a newline-bearing sibling string must not be able
    // to fabricate an additional `key: value` line.
    let v = json!({
        "items": [{"id": "a", "kind": "x"}, {"id": "b", "kind": "y"}],
        "note": "line one\nforged_key: forged_value",
        "has_more": true
    });
    let out = render_format(v, OutputFormat::Auto, PresentationMode::Agent);
    assert!(
        !out.contains("\nforged_key: forged_value"),
        "raw newline from a sibling string must not start a new line:\n{out}"
    );
    assert!(
        out.contains(r#"note: "line one\nforged_key: forged_value""#),
        "newline-bearing sibling renders as its JSON literal:\n{out}"
    );
    assert!(out.contains("has_more: true"), "siblings still preserved");
}

#[test]
fn carriage_return_in_cell_collapses_like_newline() {
    // Escaping contract pin: cells collapse \r exactly like \n, so a
    // \r- or \r\n-bearing value cannot smuggle a raw line break into
    // the table body and forge row structure.
    let v = json!([
        {"id": "a", "note": "before\rafter"},
        {"id": "b", "note": "one\r\ntwo"}
    ]);
    let out = render_format(v, OutputFormat::Auto, PresentationMode::Agent);
    assert!(
        !out.contains('\r'),
        "no raw carriage return may survive into rendered output:\n{out:?}"
    );
    assert!(
        out.contains("before after") && out.contains("one  two"),
        "\\r and \\r\\n collapse to spaces inside cells:\n{out}"
    );
}

/// The hoist never overwrites an existing top-level sibling.
#[test]
fn redundancy_drop_hoist_does_not_overwrite_top_level() {
    let v = json!({
        "id": "abc",
        "status": "active",
        "properties": {"status": "pending"}
    });
    let reduced = apply_redundancy_drop(v, RedundancyScope::View);
    assert_eq!(
        reduced.get("status").and_then(Value::as_str),
        Some("active"),
        "existing top-level status must win"
    );
    assert_eq!(
        reduced
            .get("properties")
            .and_then(|p| p.get("status"))
            .and_then(Value::as_str),
        Some("pending"),
        "conflicting properties value must stay where it was"
    );
}

/// (b3) fallback: auto on heterogeneous/scalar value falls back to compact json.
#[test]
fn format_auto_scalar_fallback_compact_json() {
    let v = json!(42);
    let rendered = render_format(v, OutputFormat::Auto, PresentationMode::Agent);
    assert_eq!(rendered, "42");
}

/// (c) table format forces markdown table even when shape would normally be kv.
#[test]
fn format_table_forces_markdown_when_array() {
    let v = json!({
        "items": [
            {"name": "A", "score": 1},
            {"name": "B", "score": 2}
        ]
    });
    let rendered = render_format(v, OutputFormat::Table, PresentationMode::Agent);
    assert!(
        rendered.contains("|"),
        "table format must produce markdown table"
    );
    assert!(rendered.contains("name"), "must have name column");
    assert!(rendered.contains("score"), "must have score column");
}

/// (c-fallback) table format falls back to compact json when no record array found.
#[test]
fn format_table_falls_back_to_json_when_no_array() {
    let v = json!({"single": "value"});
    let rendered = render_format(v, OutputFormat::Table, PresentationMode::Agent);
    // No record array → compact JSON fallback.
    let parsed: Value = serde_json::from_str(&rendered).unwrap();
    assert_eq!(parsed["single"], json!("value"));
}

/// (d) redundancy-drop: auto/table skipped in Verbose mode (§7).
#[test]
fn format_auto_verbose_skips_redundancy_drop() {
    let v = json!({
        "full_id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
        "namespace": "local",
        "title": "test"
    });
    // In Verbose mode, redundancy drop must be skipped.
    // The value is a single object → compact JSON; full_id and namespace stay.
    let rendered = render_format(v, OutputFormat::Auto, PresentationMode::Verbose);
    assert!(
        rendered.contains("full_id"),
        "verbose must preserve full_id"
    );
    assert!(
        rendered.contains("namespace"),
        "verbose must preserve namespace"
    );
}

/// Indirect check that the redundancy pre-pass doesn't corrupt an error
/// envelope's shape; the actual ok=false bypass is enforced by
/// `render_result`, not by this pre-pass.
#[test]
fn redundancy_drop_does_not_corrupt_error_shape() {
    let v = json!({"ok": false, "error": "something failed", "namespace": "local"});
    // apply_redundancy_drop is a pure value transform with no knowledge of
    // `ok`: bypassing it for error envelopes is the caller's job
    // (render_result in server.rs). This only checks the pre-pass itself
    // doesn't lose the error field.
    let reduced = apply_redundancy_drop(v.clone(), RedundancyScope::View);
    assert!(
        reduced.get("error").is_some(),
        "redundancy drop must preserve error field"
    );
    assert_eq!(
        reduced.get("ok").and_then(Value::as_bool),
        Some(false),
        "redundancy drop must preserve ok=false"
    );
}

/// Properties dedup removes only keys that match a top-level sibling exactly.
#[test]
fn redundancy_drop_properties_dedup() {
    let v = json!({
        "id": "abc",
        "title": "Same",
        "properties": {
            "title": "Same",  // duplicate → removed
            "extra": "unique" // not at top level → kept
        }
    });
    let reduced = apply_redundancy_drop(v, RedundancyScope::View);
    let props = reduced.get("properties").expect("properties must remain");
    assert!(props.get("extra").is_some(), "unique property must be kept");
    assert!(
        props.get("title").is_none(),
        "duplicate top-level property must be removed"
    );
}

/// Cell truncation: text > 120 chars gets `...` appended.
#[test]
fn cell_text_truncates_long_values() {
    let long = "X".repeat(200);
    let v = json!([
        {"col": long.clone()},
        {"col": "short"}
    ]);
    let rendered = render_format(v, OutputFormat::Auto, PresentationMode::Agent);
    // Cell must be truncated to ~120 chars + "..."
    assert!(
        rendered.contains("..."),
        "long cell must be truncated with ..."
    );
    assert!(
        !rendered.contains(&long),
        "full long string must not appear in table"
    );
}

/// Cell truncation must not panic on multi-byte UTF-8 characters.
///
/// A string of 119 ASCII bytes followed by a 3-byte CJK character and more
/// text has `len() > 120` but byte index 120 falls inside the CJK char.
/// The old byte-slice truncation would panic; char-boundary truncation is safe.
#[test]
fn cell_text_truncation_is_utf8_safe() {
    // 119 ASCII 'a' bytes, then CJK char U+4E2D (3 bytes each), then more text.
    // Total byte length: 119 + 3 * 10 + 5 > 120, but byte 120 is inside a CJK char.
    let prefix = "a".repeat(119);
    let suffix = "中".repeat(10); // each '中' is 3 bytes
    let long_multibyte = format!("{prefix}{suffix}trailing");
    let v = json!([
        {"col": long_multibyte.clone()},
        {"col": "ok"}
    ]);
    // Must not panic — this was the bug.
    let rendered = render_format(v, OutputFormat::Auto, PresentationMode::Agent);
    assert!(
        rendered.contains("..."),
        "multibyte cell must be truncated with ..."
    );
    // The rendered string must be valid UTF-8 (no partial char slicing).
    assert!(
        std::str::from_utf8(rendered.as_bytes()).is_ok(),
        "rendered output must be valid UTF-8"
    );
}

include!("presentation_tests/timestamp_offset_tests.rs");
