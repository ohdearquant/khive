use super::*;
use crate::{record_outbox_scan, OutboxScan, OutboxScanOutcome};
use khive_runtime::{curation::OutboundHealthSummary, Namespace};

fn at(seconds: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(seconds, 0).unwrap()
}

fn empty_backlog(observed_at: DateTime<Utc>) -> OutboundHealthSummary {
    OutboundHealthSummary {
        observed_at,
        undelivered_rows: 0,
        backoff_rows: 0,
        oldest_undelivered_created_at: None,
        oldest_backoff_created_at: None,
        earliest_backoff_next_attempt_at: None,
        max_backoff_delivery_attempts: None,
    }
}

async fn report(runtime: &KhiveRuntime, token: &NamespaceToken, scan: OutboxScan) {
    record_outbox_scan(runtime, token, "email", "a@example.com", scan)
        .await
        .unwrap();
}

#[tokio::test]
async fn outbox_success_failure_and_inbound_poll_keep_separate_evidence() {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = runtime.authorize(Namespace::local()).unwrap();
    super::super::quarantine_heartbeat::handle_heartbeat(
        &runtime,
        &token,
        json!({
            "channel_kind":"email", "channel_slug":"a@example.com", "outcome":"failure",
            "poll_interval_secs":5, "error_class":"auth", "error_message":"inbound failure",
            "at": at(90).to_rfc3339(),
        }),
    )
    .await
    .unwrap();
    report(
        &runtime,
        &token,
        OutboxScan {
            completed_at: at(100),
            interval_secs: 5,
            outcome: OutboxScanOutcome::Success {
                pending_rows_seen: 2,
                pending_rows_limit: 200,
                oldest_pending_created_at: Some(at(80)),
                backlog: OutboundHealthSummary {
                    undelivered_rows: 3,
                    backoff_rows: 1,
                    oldest_undelivered_created_at: Some(at(70)),
                    oldest_backoff_created_at: Some(at(70)),
                    earliest_backoff_next_attempt_at: Some(at(200)),
                    max_backoff_delivery_attempts: Some(u64::MAX),
                    ..empty_backlog(at(99))
                },
            },
        },
    )
    .await;
    report(
        &runtime,
        &token,
        OutboxScan {
            completed_at: at(105),
            interval_secs: 5,
            outcome: OutboxScanOutcome::Failure {
                error_message: "scan failed".into(),
            },
        },
    )
    .await;
    let id =
        super::super::quarantine_heartbeat::heartbeat_note_id("local", "email", "a@example.com");
    let note = runtime
        .notes(&token)
        .unwrap()
        .get_note(id)
        .await
        .unwrap()
        .unwrap();
    let value = channel_health_to_json(&note, &at(120), 0);
    assert_eq!(value["consecutive_failures"], 1);
    assert_eq!(value["last_error"]["message"], "inbound failure");
    assert_eq!(value["last_poll_attempt_at"], at(90).to_rfc3339());
    assert_eq!(value["outbox"]["last_success_at"], at(100).to_rfc3339());
    assert_eq!(
        value["outbox"]["last_scan_completed_at"],
        at(105).to_rfc3339()
    );
    assert_eq!(value["outbox"]["last_scan_succeeded"], false);
    assert_eq!(value["outbox"]["consecutive_failures"], 1);
    assert_eq!(value["outbox"]["pending_rows_seen"], 2);
    assert_eq!(value["outbox"]["backlog_observed_at"], at(99).to_rfc3339());
    assert_eq!(value["outbox"]["undelivered_rows"], 3);
    assert_eq!(value["outbox"]["backoff_rows"], 1);
    assert_eq!(value["outbox"]["max_backoff_delivery_attempts"], u64::MAX);
    assert_eq!(value["outbox"]["oldest_undelivered_age_secs"], 50);
    assert_eq!(value["outbox"]["oldest_pending_age_secs"], 40);
    assert_eq!(value["outbox"]["stalled"], false);
    assert_eq!(
        channel_health_to_json(&note, &at(121), 0)["outbox"]["stalled"],
        true
    );

    // A later poll success must retain the entire outbox observation.
    super::super::quarantine_heartbeat::handle_heartbeat(
        &runtime,
        &token,
        json!({
            "channel_kind":"email", "channel_slug":"a@example.com", "outcome":"success",
            "poll_interval_secs":5, "at":at(110).to_rfc3339(),
        }),
    )
    .await
    .unwrap();
    let after = runtime
        .notes(&token)
        .unwrap()
        .get_note(id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        after.properties.as_ref().unwrap()["outbox"],
        note.properties.as_ref().unwrap()["outbox"]
    );

    report(
        &runtime,
        &token,
        OutboxScan {
            completed_at: at(125),
            interval_secs: 5,
            outcome: OutboxScanOutcome::Success {
                pending_rows_seen: 0,
                pending_rows_limit: 200,
                oldest_pending_created_at: None,
                backlog: empty_backlog(at(125)),
            },
        },
    )
    .await;
    let after = runtime
        .notes(&token)
        .unwrap()
        .get_note(id)
        .await
        .unwrap()
        .unwrap();
    let value = channel_health_to_json(&after, &at(125), 0);
    assert_eq!(value["outbox"]["pending_rows_seen"], 0);
    assert_eq!(value["outbox"]["backoff_rows"], 0);
    assert!(value["outbox"]["max_backoff_delivery_attempts"].is_null());
    assert!(value["outbox"]["oldest_undelivered_age_secs"].is_null());
    assert!(value["outbox"]["oldest_pending_age_secs"].is_null());
    assert_eq!(value["outbox"]["last_error"]["message"], "scan failed");
    assert_eq!(value["outbox"]["consecutive_failures"], 0);
    assert_eq!(value["last_success_at"], at(110).to_rfc3339());
}

#[tokio::test]
async fn outbox_only_channel_is_scoped_and_does_not_fabricate_inbound_poll() {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = runtime
        .authorize(Namespace::parse("tenant-a").unwrap())
        .unwrap();
    report(
        &runtime,
        &token,
        OutboxScan {
            completed_at: at(100),
            interval_secs: 5,
            outcome: OutboxScanOutcome::Success {
                pending_rows_seen: 0,
                pending_rows_limit: 200,
                oldest_pending_created_at: None,
                backlog: empty_backlog(at(100)),
            },
        },
    )
    .await;
    let health = handle_health(&runtime, &token, json!({})).await.unwrap();
    assert_eq!(health["role"], "daemon");
    assert_eq!(health["namespace"], "tenant-a");
    assert_eq!(health["channels"].as_array().unwrap().len(), 1);
    let channel = &health["channels"][0];
    assert!(channel["last_poll_attempt_at"].is_null());
    assert!(channel["consecutive_failures"].is_null());
    assert!(channel["stalled"].is_null());
    assert_eq!(channel["outbox"]["stalled"], true);
    let local = runtime.authorize(Namespace::local()).unwrap();
    assert!(
        handle_health(&runtime, &local, json!({})).await.unwrap()["channels"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn outbox_missing_invalid_and_future_clock_facts_remain_unknown() {
    assert!(outbox_health_to_json(&json!({}), &at(100)).is_none());
    for outbox in [
        json!({}),
        json!({"scan_interval_secs":0, "last_scan_completed_at":at(90).to_rfc3339()}),
        json!({"scan_interval_secs":5, "last_scan_completed_at":"invalid"}),
        json!({"scan_interval_secs":u64::MAX, "last_scan_completed_at":at(90).to_rfc3339()}),
        json!({"scan_interval_secs":5, "last_scan_completed_at":at(110).to_rfc3339(), "oldest_pending_created_at":at(110).to_rfc3339()}),
    ] {
        let projected = outbox_health_to_json(&json!({"outbox":outbox}), &at(100)).unwrap();
        assert!(projected["stalled"].is_null());
        assert!(projected["oldest_pending_age_secs"].is_null());
    }
}

#[tokio::test]
async fn outbox_rejects_invalid_page_receipts_without_writing() {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = runtime.authorize(Namespace::local()).unwrap();
    for (interval, seen, limit, oldest) in [
        (0, 0, 200, None),
        (5, 1, 0, None),
        (5, 201, 200, None),
        (5, 0, 200, Some(at(90))),
    ] {
        assert!(record_outbox_scan(
            &runtime,
            &token,
            "email",
            "a@example.com",
            OutboxScan {
                completed_at: at(100),
                interval_secs: interval,
                outcome: OutboxScanOutcome::Success {
                    pending_rows_seen: seen,
                    pending_rows_limit: limit,
                    oldest_pending_created_at: oldest,
                    backlog: empty_backlog(at(100)),
                },
            }
        )
        .await
        .is_err());
    }
    assert!(
        handle_health(&runtime, &token, json!({})).await.unwrap()["channels"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn failed_inbound_polls_report_success_age_without_health_mutation() {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let heartbeat = |outcome: &str, at: DateTime<Utc>| {
        json!({
            "channel_kind":"email", "channel_slug":"a@example.com", "outcome":outcome,
            "poll_interval_secs":5, "at":at.to_rfc3339(),
            "error_class":"transport", "error_message":"poll failed"
        })
    };
    super::super::quarantine_heartbeat::handle_heartbeat(
        &runtime,
        &token,
        heartbeat("success", at(100)),
    )
    .await
    .unwrap();
    for _ in 0..79 {
        super::super::quarantine_heartbeat::handle_heartbeat(
            &runtime,
            &token,
            heartbeat("failure", at(200)),
        )
        .await
        .unwrap();
    }
    let id =
        super::super::quarantine_heartbeat::heartbeat_note_id("local", "email", "a@example.com");
    let before = runtime
        .notes(&token)
        .unwrap()
        .get_note(id)
        .await
        .unwrap()
        .unwrap();
    let projected = channel_health_to_json(&before, &at(36100), 0);
    assert_eq!(projected["consecutive_failures"], 79);
    assert_eq!(projected["no_success_age_secs"], 36000);
    assert!(projected["stalled"].is_null());
    for _ in 0..3 {
        let health = handle_health(&runtime, &token, json!({})).await.unwrap();
        assert!(health["channels"][0]["no_success_age_secs"]
            .as_u64()
            .is_some());
    }
    let after = runtime
        .notes(&token)
        .unwrap()
        .get_note(id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_value(after).unwrap(),
        serde_json::to_value(&before).unwrap()
    );

    for value in [
        Value::Null,
        json!("invalid"),
        json!(101),
        json!(at(36101).to_rfc3339()),
    ] {
        let mut unknown = before.clone();
        unknown.properties.as_mut().unwrap()["last_success_at"] = value;
        assert!(channel_health_to_json(&unknown, &at(36100), 0)["no_success_age_secs"].is_null());
    }
    let mut missing = before;
    missing
        .properties
        .as_mut()
        .unwrap()
        .as_object_mut()
        .unwrap()
        .remove("last_success_at");
    assert!(channel_health_to_json(&missing, &at(36100), 0)["no_success_age_secs"].is_null());
    let quarantine = quarantine_only_channel_to_json("email".into(), "other".into(), 1);
    assert!(quarantine["no_success_age_secs"].is_null());
}
