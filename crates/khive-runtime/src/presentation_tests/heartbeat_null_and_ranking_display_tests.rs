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
            "consecutive_held_polls": null,
            "held_messages": null,
            "last_held_at": null,
            "last_quarantined_after_hold_at": null,
            "originals_not_retained": null,
            "last_original_not_retained_at": null,
            "last_original_not_retained_reason": null,
            "original_retention_state": null,
            "notices_failed": null,
            "last_notice_failed_at": null,
            "notice_config_error": null,
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
        "consecutive_held_polls",
        "held_messages",
        "last_held_at",
        "last_quarantined_after_hold_at",
        "originals_not_retained",
        "last_original_not_retained_at",
        "last_original_not_retained_reason",
        "original_retention_state",
        "notices_failed",
        "last_notice_failed_at",
        "notice_config_error",
    ] {
        assert_eq!(
            channel.get(field),
            Some(&Value::Null),
            "Agent presentation must preserve unknown heartbeat fact `{field}`"
        );
    }
}

// comm.health reports `outbox: null` for a channel with no recorded outbox scan
// (a legacy row or a quarantine-only identity).
#[test]
fn agent_preserves_a_null_outbox() {
    let v = json!({
        "channels": [{
            "channel_kind": "email",
            "channel_slug": "ops",
            "outbox": null,
            "quarantined_count": 1,
        }]
    });
    let out = present_with_policy(
        v,
        PresentationMode::Agent,
        NOW,
        VerbPresentationPolicy::Standard,
    );
    let channel = out["channels"][0].as_object().expect("channel object");
    assert_eq!(
        channel.get("outbox"),
        Some(&Value::Null),
        "Agent presentation must keep a null outbox: {out}"
    );
}

// Before a scan first succeeds, and on an empty pending page, the outbox
// object carries nulls that state what was not observed.
#[test]
fn agent_preserves_unknown_outbox_scan_facts() {
    let v = json!({
        "channels": [{
            "outbox": {
                "last_scan_completed_at": null,
                "last_scan_succeeded": null,
                "last_success_at": null,
                "last_failure_at": null,
                "consecutive_failures": null,
                "last_error": null,
                "pending_rows_seen": null,
                "pending_rows_limit": null,
                "oldest_pending_created_at": null,
                "scan_interval_secs": null,
                "stalled": null,
                "oldest_pending_age_secs": null,
            },
        }]
    });
    let out = present_with_policy(
        v,
        PresentationMode::Agent,
        NOW,
        VerbPresentationPolicy::Standard,
    );
    let outbox = out["channels"][0]["outbox"]
        .as_object()
        .expect("outbox object");
    for field in [
        "last_scan_completed_at",
        "last_scan_succeeded",
        "last_success_at",
        "last_failure_at",
        "consecutive_failures",
        "last_error",
        "pending_rows_seen",
        "pending_rows_limit",
        "oldest_pending_created_at",
        "scan_interval_secs",
        "stalled",
        "oldest_pending_age_secs",
    ] {
        assert_eq!(
            outbox.get(field),
            Some(&Value::Null),
            "Agent presentation must preserve unknown outbox fact `{field}`"
        );
    }
}

#[test]
fn agent_keeps_a_recorded_empty_outbox_scan() {
    let v = json!({
        "channels": [{
            "outbox": {
                "last_scan_completed_at": "2025-05-23T16:07:00Z",
                "last_scan_succeeded": true,
                "last_success_at": "2025-05-23T16:07:00Z",
                "last_failure_at": null,
                "consecutive_failures": 0,
                "last_error": null,
                "pending_rows_seen": 0,
                "pending_rows_limit": 500,
                "oldest_pending_created_at": null,
                "scan_interval_secs": 60,
                "stalled": false,
                "oldest_pending_age_secs": null,
            },
        }]
    });
    let out = present_with_policy(
        v,
        PresentationMode::Agent,
        NOW,
        VerbPresentationPolicy::Standard,
    );
    let outbox = &out["channels"][0]["outbox"];
    assert_eq!(outbox["last_scan_succeeded"], json!(true), "{out}");
    assert_eq!(outbox["consecutive_failures"], json!(0), "{out}");
    assert_eq!(outbox["pending_rows_seen"], json!(0), "{out}");
    assert_eq!(outbox["pending_rows_limit"], json!(500), "{out}");
    assert_eq!(outbox["scan_interval_secs"], json!(60), "{out}");
    assert_eq!(outbox["stalled"], json!(false), "{out}");
    assert!(outbox["last_scan_completed_at"].is_string(), "{out}");
    assert!(outbox["last_success_at"].is_string(), "{out}");
    for field in [
        "last_failure_at",
        "last_error",
        "oldest_pending_created_at",
        "oldest_pending_age_secs",
    ] {
        assert_eq!(
            outbox.get(field),
            Some(&Value::Null),
            "an empty scan must keep `{field}` null: {out}"
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
