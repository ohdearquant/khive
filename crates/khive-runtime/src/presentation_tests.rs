use super::*;
use serde_json::json;

/// A fixed "now" for deterministic tests: 2025-05-23T16:08:00Z.
const NOW: i64 = 1_748_016_480;

#[test]
fn utc_rfc3339_seconds_normalizes_offsets_and_omits_fractions() {
    for (raw, expected) in [
        (
            "2026-01-01T00:15:30.987654321+01:00",
            "2025-12-31T23:15:30Z",
        ),
        ("1969-12-31T23:59:59.999999999Z", "1969-12-31T23:59:59Z"),
    ] {
        let instant = chrono::DateTime::parse_from_rfc3339(raw).unwrap();
        assert_eq!(format_utc_rfc3339_seconds(&instant), expected, "{raw}");
    }
}

#[test]
fn micros_to_iso_preserves_chrono_range_spelling() {
    for (micros, expected) in [
        (0, "1970-01-01T00:00:00.000000Z"),
        (-1, "1969-12-31T23:59:59.999999Z"),
        (1_748_016_480_123_456, "2025-05-23T16:08:00.123456Z"),
        (-62_198_755_200_000_000, "-0001-01-01T00:00:00.000000Z"),
        (253_402_300_800_000_000, "+10000-01-01T00:00:00.000000Z"),
        (-8_334_601_228_800_000_000, "-262143-01-01T00:00:00.000000Z"),
        (8_210_266_876_799_999_999, "+262142-12-31T23:59:59.999999Z"),
    ] {
        assert!(
            chrono::DateTime::<chrono::Utc>::from_timestamp_micros(micros).is_some(),
            "{micros}"
        );
        assert_eq!(micros_to_iso(micros), expected, "{micros}");
    }
}

#[test]
fn micros_to_iso_formats_outside_chrono_range_exactly() {
    for (micros, expected) in [
        (i64::MIN, "-290308-12-21T19:59:05.224192Z"),
        (i64::MAX, "+294247-01-10T04:00:54.775807Z"),
        (-8_334_601_228_800_000_001, "-262144-12-31T23:59:59.999999Z"),
        (8_210_266_876_800_000_000, "+262143-01-01T00:00:00.000000Z"),
        (-8_898_108_636_303_876_544, "-280000-02-29T12:34:56.123456Z"),
        (8_776_940_198_400_000_001, "+280100-03-01T00:00:00.000001Z"),
    ] {
        assert!(
            chrono::DateTime::<chrono::Utc>::from_timestamp_micros(micros).is_none(),
            "{micros}"
        );
        assert_eq!(micros_to_iso(micros), expected, "{micros}");
    }
}

#[test]
fn expanded_micros_timestamps_pass_through_agent_without_relative_labels() {
    for (micros, expected) in [
        (i64::MIN, "-290308-12-21T19:59:05.224192Z"),
        (i64::MAX, "+294247-01-10T04:00:54.775807Z"),
    ] {
        let shown = present(
            json!({"items": [{"created_at": micros_to_iso(micros)}]}),
            PresentationMode::Agent,
            NOW,
        );
        assert_eq!(shown, json!({"items": [{"created_at": expected}]}));
        assert!(rfc3339_to_utc_micros(expected).is_err());
    }
}

#[test]
fn rfc3339_to_utc_micros_round_trips_and_rejects_partial_forms() {
    let micros = 1_748_016_480_000_000_i64;
    assert_eq!(rfc3339_to_utc_micros(&micros_to_iso(micros)), Ok(micros));
    // Offset spellings resolve to the same instant; whitespace tolerated.
    // The offset spelling names the same instant as NOW.
    assert_eq!(
        rfc3339_to_utc_micros(" 2025-05-23T12:08:00-04:00 "),
        Ok(micros)
    );
    assert!(rfc3339_to_utc_micros("2026-05-23").is_err());
    assert!(rfc3339_to_utc_micros("2026-05-23T16:18:00").is_err());
}

fn agent(v: Value) -> Value {
    present(v, PresentationMode::Agent, NOW)
}

#[test]
fn agent_preserves_empty_string_under_properties() {
    // ADR-045 Amendment 3 (issue #1995): a key under `properties` exists
    // only because a caller wrote it, so `""` there distinguishes
    // set-to-empty from absent and must survive Agent mode — at any
    // nesting depth under `properties`.
    let v = json!({
        "id": "a1b2c3d4",
        "summary": "",
        "properties": {"k": "", "nested": {"deep": ""}},
    });
    let out = agent(v);
    assert_eq!(out["properties"]["k"], json!(""));
    assert_eq!(out["properties"]["nested"]["deep"], json!(""));
    // Control: outside `properties` the empty-string drop still applies.
    assert!(out.get("summary").is_none());
}

#[test]
fn agent_drops_scalar_empty_properties_value() {
    // The carve-out applies only to strings nested under an object-valued
    // `properties`; a scalar `properties: ""` is the field's own value,
    // not a caller-written key under it, and drops like any empty string.
    let v = json!({
        "id": "a1b2c3d4",
        "properties": "",
    });
    let out = agent(v);
    assert!(out.get("properties").is_none());
}

#[test]
fn agent_still_drops_empty_containers_under_properties() {
    // The amendment's scope note keeps the container drop: empty arrays
    // and objects under `properties` are not carved out.
    let v = json!({
        "id": "a1b2c3d4",
        "properties": {"tags": [], "meta": {}, "kept": "x"},
    });
    let out = agent(v);
    assert!(out["properties"].get("tags").is_none());
    assert!(out["properties"].get("meta").is_none());
    assert_eq!(out["properties"]["kept"], json!("x"));
}

#[test]
fn verbose_passthrough() {
    let v = json!({"id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890", "title": "X"});
    let out = present(v.clone(), PresentationMode::Verbose, NOW);
    assert_eq!(out, v);
}

#[test]
fn agent_shortens_uuid() {
    let v = json!({"id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890"});
    let out = agent(v);
    assert_eq!(out["id"], json!("a1b2c3d4"));
}

#[test]
fn agent_drops_empty_string() {
    let v = json!({"title": "ok", "description": ""});
    let out = agent(v);
    assert!(out.get("description").is_none());
    assert_eq!(out["title"], json!("ok"));
}

#[test]
fn agent_drops_empty_array() {
    let v = json!({"tags": [], "title": "ok"});
    let out = agent(v);
    assert!(out.get("tags").is_none());
}

#[test]
fn agent_preserves_empty_list_page_arrays() {
    let v = json!({
        "items": [],
        "entities": [],
        "notes": [],
        "edges": [],
        "results": [],
        "entries": [],
        "neighbors": [],
        "next_after": null,
        "requested_limit": 10,
        "effective_limit": 10,
        "limit_clamped": false,
    });
    let out = agent(v);
    for key in EMPTY_ARRAY_PRESERVE {
        assert_eq!(out[*key], json!([]), "missing structural key {key}");
    }
    assert_eq!(out["next_after"], json!(null));
}

#[test]
fn agent_preserves_keyset_cursor_envelope_completion_signals() {
    // knowledge.list(after=…) terminal/empty page: both signals survive.
    let out = agent(json!({
        "results": [],
        "limit": 500,
        "order": "created_at ASC, id ASC",
        "next_after": null,
    }));
    assert_eq!(out["results"], json!([]));
    assert!(
        out.get("next_after").is_some_and(Value::is_null),
        "terminal cursor page must keep next_after:null: {out}"
    );
}

#[test]
fn agent_results_without_next_after_is_not_a_cursor_envelope() {
    let out = agent(json!({"results": [], "limit": 500, "title": "ordinary response"}));
    assert!(out.get("results").is_none());
}

#[test]
fn agent_still_drops_empty_arrays_outside_list_envelopes() {
    let out = agent(json!({"items": [], "entities": [], "title": "ordinary response"}));
    assert!(out.get("items").is_none());
    assert!(out.get("entities").is_none());
}

#[test]
fn agent_drops_empty_object() {
    let v = json!({"properties": {}, "title": "ok"});
    let out = agent(v);
    assert!(out.get("properties").is_none());
}

#[test]
fn agent_drops_non_lifecycle_null() {
    let v = json!({"result": null, "title": "ok"});
    let out = agent(v);
    assert!(out.get("result").is_none());
}

#[test]
fn agent_preserves_lifecycle_null() {
    let v = json!({"completed_at": null, "due_at": null, "title": "ok"});
    let out = agent(v);
    assert_eq!(out["completed_at"], json!(null));
    assert_eq!(out["due_at"], json!(null));
}

#[test]
fn agent_preserves_relationship_null() {
    let v = json!({"parent_id": null, "superseded_by": null});
    let out = agent(v);
    assert_eq!(out["parent_id"], json!(null));
    assert_eq!(out["superseded_by"], json!(null));
}

#[test]
fn agent_preserves_unknown_channel_heartbeat_nulls() {
    let v = json!({
        "channels": [{
            "poll_interval_secs": null,
            "stalled": null,
            "last_success_at": null,
            "last_poll_attempt_at": null,
            "last_failure_at": null,
            "last_error": null,
            "consecutive_failures": null,
            "quarantined_count": 1,
        }]
    });
    let out = agent(v);
    let channel = out["channels"][0].as_object().expect("channel object");
    for field in [
        "poll_interval_secs",
        "stalled",
        "last_success_at",
        "last_poll_attempt_at",
        "last_failure_at",
        "last_error",
        "consecutive_failures",
    ] {
        assert_eq!(
            channel.get(field),
            Some(&Value::Null),
            "Agent presentation must preserve unknown heartbeat fact `{field}`"
        );
    }
}

#[test]
fn agent_truncates_score_field() {
    let v = json!({"score": 0.12345678});
    let out = agent(v);
    let s = out["score"].as_f64().unwrap();
    assert!((s - 0.123).abs() < 1e-9, "expected ~0.123, got {s}");
}

#[test]
fn agent_presents_ranking_alias_and_nested_evidence() {
    let canonical = json!([
        {
            "rank_score": 0.0325224749,
            "score": 0.0325224749,
            "rank_score_kind": "rrf",
            "signals": {"vector_similarity": 0.842567, "keyword_score": 18.375}
        },
        {"rank_score": 0.0, "score": 0.0, "rank_score_kind": "keyword", "signals": {}}
    ]);
    for mode in [PresentationMode::Verbose, PresentationMode::Human] {
        assert_eq!(present(canonical.clone(), mode, NOW), canonical);
    }
    let shown = agent(canonical);
    assert_eq!(shown[0]["rank_score"], json!(0.0325));
    assert_eq!(shown[0]["score"], shown[0]["rank_score"]);
    assert_eq!(shown[0]["rank_score_kind"], "rrf");
    assert_eq!(shown[0]["signals"]["vector_similarity"], json!(0.843));
    assert_eq!(shown[0]["signals"]["keyword_score"], json!(18.4));
    assert_eq!(shown[1]["rank_score"], json!(0.0));
    assert_eq!(shown[1]["score"], shown[1]["rank_score"]);
    assert_eq!(shown[1]["rank_score_kind"], "keyword");
    assert!(shown[1].get("signals").is_none());
}

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

// ── ADR-078: OutputFormat tests ───────────────────────────────────────────

/// (a) verbose json preserves full shape (no field dropped, no transformation).
#[test]
fn format_verbose_json_preserves_full_shape() {
    let v = json!({
        "full_id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
        "namespace": "local",
        "properties": {"k": "v"},
        "title": "test"
    });
    let rendered = render_format(v.clone(), OutputFormat::Json, PresentationMode::Verbose);
    let parsed: Value = serde_json::from_str(&rendered).unwrap();
    // full_id must not be dropped in json mode.
    assert!(
        parsed.get("full_id").is_some(),
        "json mode must keep full_id"
    );
    // namespace must NOT be elided in json mode.
    assert_eq!(
        parsed.get("namespace").and_then(Value::as_str),
        Some("local")
    );
    // properties must NOT be deduped in json mode.
    assert!(parsed.get("properties").is_some());
}

/// (a-agent-json) Agent JSON applies the Machine-scope redundancy
/// reduction: `namespace`/duplicate-`properties` elision, but `full_id`
/// is retained — it is the caller's chaining handle, not a view drop
/// (ADR-078 Amendment 3, corrected).
#[test]
fn format_agent_json_drops_redundancy() {
    let v = json!({
        "full_id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
        "namespace": "local",
        "title": "test",
        "status": "next",
        "properties": {"status": "next", "additive": true}
    });
    let rendered = render_format(v, OutputFormat::Json, PresentationMode::Agent);
    let parsed: Value = serde_json::from_str(&rendered).unwrap();
    assert!(
        parsed.get("full_id").is_some(),
        "Agent JSON must keep full_id: it is the machine contract's chaining handle"
    );
    assert!(parsed.get("namespace").is_none());
    assert_eq!(parsed["status"], json!("next"));
    assert!(parsed["properties"].get("status").is_none());
    assert_eq!(parsed["properties"]["additive"], json!(true));
}

/// Agent JSON (Machine scope) keeps `full_id` while dropping
/// `namespace="local"` and duplicate `properties`; Auto (View scope) on
/// the same record still drops all three — the view scope is unchanged
/// by Amendment 3's correction.
#[test]
fn agent_json_keeps_full_id_while_auto_still_drops_it() {
    let v = json!({
        "full_id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
        "namespace": "local",
        "title": "test",
        "properties": {"title": "test", "additive": true}
    });

    let json_rendered = render_format(v.clone(), OutputFormat::Json, PresentationMode::Agent);
    let json_parsed: Value = serde_json::from_str(&json_rendered).unwrap();
    assert!(
        json_parsed.get("full_id").is_some(),
        "Agent JSON must keep full_id"
    );
    assert!(
        json_parsed.get("namespace").is_none(),
        "Agent JSON must elide namespace=\"local\""
    );
    assert!(
        json_parsed["properties"].get("title").is_none(),
        "Agent JSON must dedupe a duplicate properties entry"
    );
    assert_eq!(json_parsed["properties"]["additive"], json!(true));

    let auto_rendered = render_format(v, OutputFormat::Auto, PresentationMode::Agent);
    assert!(
        !auto_rendered.contains("full_id"),
        "Auto (view scope) must still drop full_id: {auto_rendered}"
    );
    assert!(
        !auto_rendered.contains("\"namespace\""),
        "Auto must still elide namespace=\"local\": {auto_rendered}"
    );
}

/// (b1) homogeneous record array → markdown table with header + separator + rows.
#[test]
fn format_auto_homogeneous_array_renders_markdown_table() {
    let v = json!([
        {"id": "abc", "title": "First"},
        {"id": "def", "title": "Second"}
    ]);
    let rendered = render_format(v, OutputFormat::Auto, PresentationMode::Agent);
    assert!(rendered.starts_with('|'), "must start with |");
    assert!(
        rendered.contains("| id |") || rendered.contains("| id"),
        "must have id column"
    );
    assert!(rendered.contains("title"), "must have title column");
    assert!(rendered.contains("|---|"), "must have separator row");
    assert!(rendered.contains("abc"), "must have first row data");
    assert!(rendered.contains("Second"), "must have second row data");
}

/// (b2) single record → compact JSON, lossless (kv-block renderer removed).
#[test]
fn format_auto_single_record_renders_compact_json() {
    let v = json!({"id": "abc", "title": "Hello World"});
    let rendered = render_format(v.clone(), OutputFormat::Auto, PresentationMode::Agent);
    let parsed: Value = serde_json::from_str(&rendered).expect("must be valid JSON");
    assert_eq!(parsed, v, "single record must round-trip losslessly");
    assert!(
        !rendered.starts_with('|'),
        "single record must not be a markdown table"
    );
}

/// (b2-lossless) a single record with a large payload field (a compose
/// briefing's `markdown`) arrives whole — the former kv-block renderer
/// truncated it.
#[test]
fn format_auto_single_record_large_payload_survives_whole() {
    let briefing = "line one\nline two\n".repeat(500); // ~9KB, newlines included
    let v = json!({"id": "abc", "markdown": briefing.clone()});
    let rendered = render_format(v, OutputFormat::Auto, PresentationMode::Agent);
    let parsed: Value = serde_json::from_str(&rendered).expect("must be valid JSON");
    assert_eq!(
        parsed.get("markdown").and_then(Value::as_str),
        Some(briefing.as_str()),
        "large payload field must survive untruncated"
    );
}

/// Envelope siblings survive table rendering: a `query`-style page whose
/// `has_more` is dropped reads as complete — fail-open.
#[test]
fn format_auto_table_preserves_sibling_scalars() {
    let v = json!({
        "results": [
            {"id": "abc", "title": "First"},
            {"id": "def", "title": "Second"}
        ],
        "has_more": true,
        "offset": 20,
        "page_size": 2,
        "truncated": false
    });
    let rendered = render_format(v, OutputFormat::Auto, PresentationMode::Agent);
    assert!(rendered.contains("| id"), "records must render as a table");
    assert!(
        rendered.contains("has_more: true"),
        "has_more sibling must survive: {rendered}"
    );
    assert!(
        rendered.contains("offset: 20"),
        "offset sibling must survive"
    );
    assert!(
        rendered.contains("page_size: 2"),
        "page_size sibling must survive"
    );
    assert!(
        rendered.contains("truncated: false"),
        "truncated sibling must survive"
    );
}

/// Nested table cells render a constant elision marker, never
/// truncated pseudo-JSON that reads as data while missing fields.
#[test]
fn format_auto_nested_cell_renders_elision_marker() {
    let big_nested: Value = json!({"k": "v".repeat(300), "other": {"deep": true}});
    let v = json!([
        {"id": "abc", "properties": big_nested},
        {"id": "def", "properties": {"x": 1}}
    ]);
    let rendered = render_format(v, OutputFormat::Auto, PresentationMode::Agent);
    assert!(
        rendered.contains("{…}"),
        "nested object cell must render the elision marker: {rendered}"
    );
    assert!(
        !rendered.contains("..."),
        "no truncated pseudo-JSON in nested cells"
    );
}

/// Arrays of scalars (tags) still render as compact JSON in cells.
#[test]
fn format_auto_scalar_array_cell_renders_compact_json() {
    let v = json!([
        {"id": "abc", "tags": ["lesson", "khive"]},
        {"id": "def", "tags": ["adr"]}
    ]);
    let rendered = render_format(v, OutputFormat::Auto, PresentationMode::Agent);
    assert!(
        rendered.contains(r#"["lesson","khive"]"#),
        "scalar array cell must be compact JSON: {rendered}"
    );
}

/// Identity/decision columns are exempt from truncation: a truncated
/// title cannot be selected on, a truncated signature is unusable.
#[test]
fn format_auto_identity_columns_not_truncated() {
    let long_title = "T".repeat(300);
    let long_note = "N".repeat(300);
    let v = json!([
        {"id": "abc", "title": long_title.clone(), "note": long_note.clone()},
        {"id": "def", "title": "short", "note": "short"}
    ]);
    let rendered = render_format(v, OutputFormat::Auto, PresentationMode::Agent);
    assert!(
        rendered.contains(&long_title),
        "title column must not be truncated"
    );
    assert!(
        !rendered.contains(&long_note),
        "non-exempt column must still truncate"
    );
}

/// Verbose presentation disables cell truncation entirely (ADR-078 §3a).
#[test]
fn format_auto_verbose_disables_truncation() {
    let long_note = "N".repeat(300);
    let v = json!([
        {"id": "abc", "note": long_note.clone()},
        {"id": "def", "note": "short"}
    ]);
    let rendered = render_format(v, OutputFormat::Auto, PresentationMode::Verbose);
    assert!(
        rendered.contains(&long_note),
        "verbose must render full cell content"
    );
}

/// The pre-pass hoists named scalar payload fields (`trigger_at`, `due`,
/// `status`) out of `properties` so they can surface as table columns —
/// a scheduled event carries them only inside `properties`.
#[test]
fn table_path_hoists_payload_scalars_to_columns() {
    // The hoist is table-path only (ADR-078 Amendment 2): two scheduled
    // events whose trigger_at/status live inside `properties` must gain
    // top-level columns in the rendered table.
    let record = |n: u32| {
        json!({
            "id": format!("evt-{n}"),
            "properties": {
                "trigger_at": "2026-09-01T14:00:00-04:00",
                "status": "pending",
                "dispatch_receipt": {"state": "succeeded"}
            }
        })
    };
    let out = render_format(
        json!([record(1), record(2)]),
        OutputFormat::Auto,
        PresentationMode::Agent,
    );
    let header = out.lines().next().expect("table header");
    assert!(
        header.contains("trigger_at") && header.contains("status"),
        "hoisted scalars must appear as table columns, got header: {header}"
    );
    assert!(
        out.contains("2026-09-01T14:00:00-04:00") && out.contains("pending"),
        "hoisted values must render in cells:\n{out}"
    );
}

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

// --- parse_iso8601_unix / relative-time offset handling ---

#[test]
fn parse_iso8601_unix_negative_offset_matches_equivalent_utc() {
    // "-04:00" is 4 hours behind UTC, so 11:55 local == 15:55Z.
    assert_eq!(
        parse_iso8601_unix("2026-07-09T11:55:00-04:00"),
        parse_iso8601_unix("2026-07-09T15:55:00Z")
    );
}

#[test]
fn parse_iso8601_unix_positive_offset_matches_equivalent_utc() {
    // "+04:00" is 4 hours ahead of UTC, so 20:15 local == 16:15Z.
    assert_eq!(
        parse_iso8601_unix("2026-05-23T20:15:00+04:00"),
        parse_iso8601_unix("2026-05-23T16:15:00Z")
    );
}

#[test]
fn parse_iso8601_unix_zero_offset_matches_z() {
    assert_eq!(
        parse_iso8601_unix("2026-07-09T15:55:00+00:00"),
        parse_iso8601_unix("2026-07-09T15:55:00Z")
    );
}

#[test]
fn parse_iso8601_unix_compact_offset_form_matches_colon_form() {
    assert_eq!(
        parse_iso8601_unix("2026-07-09T11:55:00-0400"),
        parse_iso8601_unix("2026-07-09T11:55:00-04:00")
    );
}

#[test]
fn parse_iso8601_unix_fractional_seconds_with_offset() {
    // Fractional seconds are dropped (whole-second precision only) but
    // must not prevent the trailing offset from being applied.
    assert_eq!(
        parse_iso8601_unix("2026-07-09T11:55:00.123-04:00"),
        parse_iso8601_unix("2026-07-09T15:55:00Z")
    );
}

#[test]
fn parse_iso8601_unix_fractional_seconds_with_z() {
    assert_eq!(
        parse_iso8601_unix("2026-07-09T15:55:00.999Z"),
        parse_iso8601_unix("2026-07-09T15:55:00Z")
    );
}

#[test]
fn parse_iso8601_unix_rejects_bare_form() {
    assert_eq!(parse_iso8601_unix("2026-07-09T15:55:00"), None);
}

#[test]
fn parse_iso8601_unix_malformed_tail_returns_none() {
    assert_eq!(parse_iso8601_unix("2026-07-09T15:55:00X"), None);
    assert_eq!(parse_iso8601_unix("2026-07-09T15:55:00+04"), None);
    assert_eq!(parse_iso8601_unix("2026-07-09T15:55:00."), None);
}

#[test]
fn parse_iso8601_unix_out_of_range_offset_returns_none() {
    // Hour out of range (>23), colon and compact forms.
    assert_eq!(parse_iso8601_unix("2026-07-09T15:55:00+24:00"), None);
    assert_eq!(parse_iso8601_unix("2026-07-09T15:55:00+2400"), None);
    // Minute out of range (>59), colon and compact forms.
    assert_eq!(parse_iso8601_unix("2026-07-09T15:55:00+01:60"), None);
    assert_eq!(parse_iso8601_unix("2026-07-09T15:55:00+0160"), None);
}

#[test]
fn parse_iso8601_unix_max_valid_offset_boundary_is_accepted() {
    // +23:59 / -23:59 are the largest valid offsets and must still parse.
    assert!(parse_iso8601_unix("2026-07-09T15:55:00+23:59").is_some());
    assert!(parse_iso8601_unix("2026-07-09T15:55:00-23:59").is_some());
    assert!(parse_iso8601_unix("2026-07-09T15:55:00+2359").is_some());
}

#[test]
fn exact_timestamp_converts_future_offset_without_relative_form() {
    let out = exact_timestamp("2025-05-23T16:08:00-02:00").unwrap();
    assert_eq!(out.0, "2025-05-23T18:08:00Z");
    assert!(out.1 > NOW);
}

#[test]
fn exact_timestamp_converts_past_offset_and_retains_fraction() {
    let out = exact_timestamp("2025-05-23T20:05:00.123+04:00").unwrap();
    assert_eq!(out.0, "2025-05-23T16:05:00.123Z");
    assert_eq!(out.1, NOW - 180);
}
