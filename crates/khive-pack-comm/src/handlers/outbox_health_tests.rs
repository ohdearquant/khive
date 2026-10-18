use super::*;
use crate::{record_outbox_scan, OutboxScan, OutboxScanOutcome};
use khive_runtime::Namespace;

fn at(seconds: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(seconds, 0).unwrap()
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
                    oldest_pending_created_at: oldest
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
