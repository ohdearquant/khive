use super::*;
use khive_runtime::RuntimeConfig;

fn at(value: &str) -> DateTime<Utc> {
    value.parse().expect("fixture timestamp")
}

async fn fixture() -> (tempfile::TempDir, KhiveRuntime) {
    let dir = tempfile::tempdir().expect("temporary database");
    let rt = KhiveRuntime::new(RuntimeConfig {
        db_path: Some(dir.path().join("misfire.db")),
        default_namespace: Namespace::local(),
        embedding_model: None,
        additional_embedding_models: vec![],
        packs: vec!["kg".into(), "schedule".into(), "comm".into()],
        ..Default::default()
    })
    .expect("runtime");
    (dir, rt)
}

async fn create_event(rt: &KhiveRuntime, properties: Value) -> uuid::Uuid {
    let token = rt.authorize(Namespace::local()).expect("authorize");
    let note = rt
        .create_note(
            &token,
            "scheduled_event",
            None,
            "misfire fixture",
            None,
            Some(properties),
            vec![],
        )
        .await
        .expect("create fixture");
    let provenance = khive_storage::Event::new(
        "local",
        khive_pack_schedule::CREATOR_PROVENANCE_VERB,
        EventKind::Audit,
        SubstrateKind::Note,
        format!("{}:{}", token.actor().kind, token.actor().id),
    )
    .with_target(note.id)
    .with_payload(json!({
        "provenance": khive_pack_schedule::CREATOR_PROVENANCE_MARKER_V1,
        "event_type": "schedule",
    }));
    rt.events(&token)
        .expect("events")
        .append_event(provenance)
        .await
        .expect("provenance");
    note.id
}

async fn properties(rt: &KhiveRuntime, id: uuid::Uuid) -> Value {
    serde_json::from_str(
        &current_note_properties_text(rt, "local", id)
            .await
            .expect("read properties")
            .expect("event exists"),
    )
    .expect("JSON")
}

async fn replace_properties(rt: &KhiveRuntime, id: uuid::Uuid, value: Value) {
    let token = rt.authorize(Namespace::local()).expect("authorize");
    assert!(rt
        .notes(&token)
        .expect("notes")
        .update_note_properties(id, Some(value), Utc::now().timestamp_micros())
        .await
        .expect("replace fixture properties"));
}

// Required compatibility arm comes before every v2 fixture. Policy metadata
// added later cannot reinterpret an already durable version-1 occurrence.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn version_one_recovery_keeps_original_occurrence_and_legacy_advancement() {
    let (_dir, rt) = fixture().await;
    let source = "2000-01-01T09:00:00Z";
    let id = create_event(
        &rt,
        json!({
            "trigger_at": source, "status": "pending", "event_type": "schedule",
            "payload": "stats()", "repeat": "daily", "fired_at": null,
            "misfire_policy": "catch_up_once", "max_lateness": 3600,
        }),
    )
    .await;
    let claim = claim_pending_event(
        &rt,
        "local",
        id,
        dispatch_occurrence_id(id, at(source)),
        source,
        "anonymous:local",
        DispatchLeaseConfig::from_env(),
    )
    .await
    .expect("claim")
    .expect("claimed");
    assert!(
        mark_dispatch_invoking(&rt, "local", id, &claim, DispatchLeaseConfig::from_env())
            .await
            .expect("invoking")
    );
    let receipt =
        persist_dispatch_outcome(&rt, "local", id, &claim, &DispatchCompletion::Succeeded)
            .await
            .expect("outcome")
            .expect("stored");
    assert_eq!(receipt["version"], 1);
    assert!(receipt.get("scheduled_for").is_none());
    let recovered = reclaim_stale_firing_events(&rt, Utc::now().timestamp_micros() + 1_000_000)
        .await
        .expect("recover");
    assert_eq!(recovered.advanced, 1);
    let saved = properties(&rt, id).await;
    assert_eq!(saved["trigger_at"], "2000-01-02T09:00:00+00:00");
    assert_eq!(saved["dispatch_receipt"], receipt);
    assert_eq!(
        saved["dispatch_receipt"]["occurrence_id"],
        dispatch_occurrence_id(id, at(source)).to_string()
    );
}

fn clock() -> DateTime<Utc> {
    DateTime::from_timestamp(Utc::now().timestamp(), 0).expect("current second")
}

fn catch_up_properties(source: &str, repeat: Option<&str>, bound: i64) -> Value {
    json!({
        "trigger_at": source, "status": "pending", "event_type": "schedule",
        "payload": "create(kind=\"observation\", content=\"catch-up side effect\")",
        "repeat": repeat, "fired_at": null, "misfire_policy": "catch_up_once",
        "max_lateness": bound,
    })
}

async fn effects(rt: &KhiveRuntime) -> usize {
    let token = rt.authorize(Namespace::local()).expect("authorize");
    rt.notes(&token)
        .expect("notes")
        .query_notes(
            "local",
            Some("observation"),
            khive_storage::types::PageRequest {
                limit: 200,
                offset: 0,
            },
        )
        .await
        .expect("observations")
        .items
        .into_iter()
        .filter(|note| note.content == "catch-up side effect")
        .count()
}

async fn drain_at(rt: &KhiveRuntime, now: DateTime<Utc>) -> DrainSummary {
    let server = KhiveMcpServer::new(rt.clone()).expect("server");
    MISFIRE_NOW
        .scope(now, run_pending_events_on(rt, &server, false))
        .await
        .expect("drain")
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn old_backup_dispatches_only_latest_daily_occurrence_and_never_bursts() {
    let (_dir, rt) = fixture().await;
    let now = clock();
    let source = (now - Duration::days(30) - Duration::minutes(10)).to_rfc3339();
    let latest = now - Duration::minutes(10);
    let id = create_event(&rt, catch_up_properties(&source, Some("daily"), 3600)).await;
    let summary = drain_at(&rt, now).await;
    assert_eq!((summary.invoked, summary.advanced), (1, 1));
    assert_eq!(effects(&rt).await, 1);
    let saved = properties(&rt, id).await;
    assert_eq!(saved["status"], "pending");
    assert_eq!(
        saved["trigger_at"],
        (latest + Duration::days(1)).to_rfc3339()
    );
    assert!(saved["fired_at"].as_str().is_some());
    assert_eq!(saved["missed_at"], now.timestamp_micros());
    let receipt = &saved["dispatch_receipt"];
    assert_eq!(receipt["version"], 2);
    assert_eq!(receipt["source_trigger_at"], source);
    assert_eq!(receipt["scheduled_for"], latest.to_rfc3339());
    assert_eq!(receipt["decision_at"], now.timestamp_micros());
    assert_eq!(receipt["effective_max_lateness"], 3600);
    assert_eq!(
        receipt["occurrence_id"],
        dispatch_occurrence_id(id, latest).to_string()
    );
    assert_ne!(
        receipt["occurrence_id"],
        dispatch_occurrence_id(id, at(&source)).to_string()
    );
    assert!(
        validate_dispatch_receipt(
            id,
            receipt["claimed_at"].as_i64().unwrap(),
            &saved,
            receipt.clone()
        )
        .is_ok(),
        "finalized receipt retains L after source advances"
    );
    assert_eq!(drain_at(&rt, now).await.invoked, 0);
    assert_eq!(effects(&rt).await, 1);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn explicit_horizon_admits_equality_and_rejects_even_inside_global_grace() {
    for (age, bound, repeat, invoked) in [
        (2, 2, None, true),
        (3, 2, None, false),
        (3, 2, Some("daily"), false),
    ] {
        let (_dir, rt) = fixture().await;
        let now = clock();
        let source = (now - Duration::seconds(age)).to_rfc3339();
        let mut value = catch_up_properties(&source, repeat, bound);
        if repeat.is_some() {
            value["fired_at"] = json!("1999-01-01T00:00:00Z");
        }
        let id = create_event(&rt, value).await;
        let summary = drain_at(&rt, now).await;
        assert_eq!(summary.invoked, if invoked { 1 } else { 0 });
        assert_eq!(effects(&rt).await, if invoked { 1 } else { 0 });
        let saved = properties(&rt, id).await;
        if invoked {
            assert_eq!(saved["status"], "fired");
            assert!(saved["fired_at"].as_str().is_some());
        } else {
            assert_eq!(summary.missed, vec![id]);
            // A miss records no fire: a repeat keeps its last real fire time.
            if repeat.is_some() {
                assert_eq!(saved["fired_at"], "1999-01-01T00:00:00Z");
            } else {
                assert!(saved["fired_at"].is_null());
            }
            assert_eq!(saved["dispatch_receipt"]["state"], "missed");
            assert_eq!(
                saved["status"],
                if repeat.is_some() {
                    "pending"
                } else {
                    "missed"
                }
            );
            if repeat.is_some() {
                assert_eq!(
                    saved["trigger_at"],
                    (at(&source) + Duration::days(1)).to_rfc3339()
                );
            }
        }
    }
}

#[test]
fn decision_microseconds_are_normalized_before_selection_and_due_check() {
    let source = "2026-01-01T00:00:00.000000500Z";
    let value = catch_up_properties(source, Some("every:1s"), 1);
    assert!(select_catch_up_occurrence(&value, source, at(source), 1)
        .unwrap()
        .is_none());
    let now = at("2026-01-01T00:00:01.000000999Z");
    let selected = select_catch_up_occurrence(&value, source, now, 1)
        .unwrap()
        .unwrap();
    assert_eq!(
        selected.decision_at,
        at("2026-01-01T00:00:01Z").timestamp_micros()
    );
    assert_eq!(selected.scheduled_for, at(source));
    assert!(!selected.is_missed());
}

#[test]
fn latest_selection_preserves_off_pattern_cron_initial_and_monthly_anchor() {
    let source = "2026-01-05T08:30:00+00:00";
    let value = catch_up_properties(source, Some("0 9 * * *"), 3600);
    let selected = select_catch_up_occurrence(&value, source, at("2026-01-05T08:45:00Z"), 3600)
        .unwrap()
        .unwrap();
    assert_eq!(selected.scheduled_for, at(source));
    let source = "2026-02-28T12:00:00Z";
    let mut monthly = catch_up_properties(source, Some("monthly"), 3600);
    monthly["repeat_anchor"] = json!("2026-01-31T12:00:00Z");
    let selected = select_catch_up_occurrence(&monthly, source, at("2026-03-31T12:10:00Z"), 3600)
        .unwrap()
        .unwrap();
    assert_eq!(selected.scheduled_for, at("2026-03-31T12:00:00Z"));
    let mut legacy = catch_up_properties("2026-01-31T12:00:00Z", Some("monthly"), 3600);
    let next = advance_repeat_past_missed_for_event(
        &mut legacy,
        &Some("monthly".into()),
        at("2026-01-31T12:00:00Z"),
        at("2026-03-31T12:10:00Z"),
    )
    .unwrap()
    .unwrap();
    assert_eq!(next, at("2026-04-30T12:00:00Z"));
    assert_eq!(legacy["repeat_anchor"], "2026-01-31T12:00:00Z");
    monthly["repeat_anchor"] = json!("2027-01-31T12:00:00Z");
    assert!(
        select_catch_up_occurrence(&monthly, source, at("2026-03-31T12:10:00Z"), 3600).is_err()
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn corrupt_policy_and_recurrence_fail_not_invoked_without_side_effects() {
    let invalid = [
        ("misfire_policy", json!("unknown")),
        ("misfire_policy", Value::Null),
        ("max_lateness", json!(0)),
        ("max_lateness", json!("30")),
        ("repeat", json!(true)),
        ("repeat", json!("never")),
        ("repeat_anchor", json!("not a timestamp")),
    ];
    for (field, bad) in invalid {
        let (_dir, rt) = fixture().await;
        let now = clock();
        let source = (now - Duration::seconds(5)).to_rfc3339();
        let mut value = catch_up_properties(&source, Some("monthly"), 60);
        value[field] = bad;
        let id = create_event(&rt, value).await;
        let summary = drain_at(&rt, now).await;
        assert_eq!(summary.invoked, 0, "{field}");
        assert_eq!(effects(&rt).await, 0);
        let saved = properties(&rt, id).await;
        assert_eq!(saved["status"], "failed");
        assert_eq!(saved["dispatch_receipt"]["state"], "not_invoked");
        assert!(saved["dispatch_receipt"]["error"].as_str().is_some());
    }
}

async fn claim_catch_up(rt: &KhiveRuntime, id: uuid::Uuid, now: DateTime<Utc>) -> DispatchClaim {
    let value = properties(rt, id).await;
    let source = value["trigger_at"].as_str().unwrap();
    let selected =
        select_catch_up_occurrence(&value, source, now, value["max_lateness"].as_i64().unwrap())
            .expect("selection")
            .expect("due");
    claim_pending_catch_up_event(
        rt,
        "local",
        id,
        selected,
        source,
        "anonymous:local",
        DispatchLeaseConfig::from_env(),
    )
    .await
    .expect("claim")
    .expect("claimed")
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn durable_success_recovers_same_selected_occurrence_without_second_action() {
    let (_dir, rt) = fixture().await;
    let server = KhiveMcpServer::new(rt.clone()).expect("server");
    let now = clock();
    let source = (now - Duration::days(3) - Duration::minutes(10)).to_rfc3339();
    let initial = catch_up_properties(&source, Some("daily"), 3600);
    let action = initial["payload"].as_str().unwrap().to_string();
    let id = create_event(&rt, initial).await;
    let claim = claim_catch_up(&rt, id, now).await;
    assert!(
        mark_dispatch_invoking(&rt, "local", id, &claim, DispatchLeaseConfig::from_env())
            .await
            .unwrap()
    );
    dispatch_action(&action, "local", None, &server, false)
        .await
        .expect("actual action");
    let receipt =
        persist_dispatch_outcome(&rt, "local", id, &claim, &DispatchCompletion::Succeeded)
            .await
            .unwrap()
            .unwrap();
    assert_eq!(effects(&rt).await, 1);
    // Crash here: durable outcome exists, lifecycle finalization has not run.
    let recovered_at = now + Duration::days(4);
    let summary = reclaim_stale_firing_events(&rt, recovered_at.timestamp_micros())
        .await
        .unwrap();
    assert_eq!((summary.rows, summary.advanced), (1, 1));
    let saved = properties(&rt, id).await;
    assert_eq!(saved["dispatch_receipt"], receipt);
    assert_eq!(
        saved["trigger_at"],
        (recovered_at + Duration::days(1) - Duration::minutes(10)).to_rfc3339()
    );
    assert_eq!(
        saved["dispatch_receipt"]["scheduled_for"],
        (now - Duration::minutes(10)).to_rfc3339()
    );
    assert_eq!(drain_at(&rt, now).await.invoked, 0);
    assert_eq!(effects(&rt).await, 1);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn expired_claim_reselects_but_invoking_and_corrupt_receipts_never_replay() {
    for state in ["claimed", "invoking", "corrupt"] {
        let (_dir, rt) = fixture().await;
        let now = clock();
        let source = (now - Duration::days(2) - Duration::minutes(10)).to_rfc3339();
        let id = create_event(&rt, catch_up_properties(&source, Some("daily"), 3600)).await;
        let claim = claim_catch_up(&rt, id, now).await;
        if state == "invoking" {
            assert!(mark_dispatch_invoking(
                &rt,
                "local",
                id,
                &claim,
                DispatchLeaseConfig::from_env()
            )
            .await
            .unwrap());
        }
        if state == "corrupt" {
            let mut value = properties(&rt, id).await;
            value["dispatch_receipt"]["scheduled_for"] = json!(source);
            replace_properties(&rt, id, value).await;
        }
        let later = now + Duration::days(1);
        let recovered = reclaim_stale_firing_events(&rt, later.timestamp_micros())
            .await
            .unwrap();
        let saved = properties(&rt, id).await;
        assert_eq!(effects(&rt).await, 0);
        if state == "claimed" {
            assert_eq!(recovered.retry_pending, 1);
            assert_eq!(saved["status"], "pending");
            let summary = drain_at(&rt, later).await;
            assert_eq!(summary.invoked, 1);
            let saved = properties(&rt, id).await;
            assert_ne!(
                saved["dispatch_receipt"]["invocation_id"],
                claim.invocation_id.to_string()
            );
            assert_eq!(
                saved["dispatch_receipt"]["scheduled_for"],
                (later - Duration::minutes(10)).to_rfc3339()
            );
            assert_eq!(effects(&rt).await, 1);
        } else {
            assert_eq!(recovered.indeterminate, 1);
            assert_eq!(saved["status"], "failed");
            assert_eq!(saved["dispatch_receipt"]["state"], "indeterminate");
            assert_eq!(drain_at(&rt, later).await.invoked, 0);
        }
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn known_failure_consumes_repeat_but_one_shot_retries_with_a_fresh_bound() {
    for repeat in [None, Some("daily")] {
        let (_dir, rt) = fixture().await;
        let now = clock();
        let source = (now - Duration::seconds(2)).to_rfc3339();
        let mut value = catch_up_properties(&source, repeat, 10);
        value["payload"] = json!("not a valid DSL(");
        let id = create_event(&rt, value).await;
        let summary = drain_at(&rt, now).await;
        assert_eq!(summary.invoked, 1);
        assert_eq!(effects(&rt).await, 0);
        let saved = properties(&rt, id).await;
        assert_eq!(saved["dispatch_receipt"]["state"], "failed");
        assert!(saved["dispatch_error"].as_str().is_some());
        assert_eq!(saved["status"], "pending");
        if repeat.is_some() {
            assert_eq!(summary.advanced, 1);
            assert_eq!(
                saved["trigger_at"],
                (at(&source) + Duration::days(1)).to_rfc3339()
            );
            assert_eq!(drain_at(&rt, now).await.invoked, 0);
        } else {
            assert_eq!(summary.retry_pending, 1);
            let next = drain_at(&rt, now + Duration::seconds(20)).await;
            assert_eq!(next.invoked, 0);
            assert_eq!(next.missed, vec![id]);
            assert_eq!(
                properties(&rt, id).await["dispatch_receipt"]["state"],
                "missed"
            );
        }
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn recovery_of_known_failure_advances_but_ambiguous_failure_is_terminal() {
    for ambiguous in [false, true] {
        let (_dir, rt) = fixture().await;
        let now = clock();
        let source = (now - Duration::days(2) - Duration::minutes(1)).to_rfc3339();
        let id = create_event(&rt, catch_up_properties(&source, Some("daily"), 3600)).await;
        let claim = claim_catch_up(&rt, id, now).await;
        assert!(
            mark_dispatch_invoking(&rt, "local", id, &claim, DispatchLeaseConfig::from_env())
                .await
                .unwrap()
        );
        let failure = if ambiguous {
            DispatchFailure::with_payload(
                "commit outcome unknown",
                json!({"domain_disposition":"unknown"}),
            )
        } else {
            DispatchFailure::plain("known failure before any action effect")
        };
        persist_dispatch_outcome(
            &rt,
            "local",
            id,
            &claim,
            &DispatchCompletion::Failed(failure),
        )
        .await
        .unwrap()
        .unwrap();
        let later = now + Duration::days(3);
        let summary = reclaim_stale_firing_events(&rt, later.timestamp_micros())
            .await
            .unwrap();
        let saved = properties(&rt, id).await;
        if ambiguous {
            assert_eq!(summary.indeterminate, 1);
            assert_eq!(saved["status"], "failed");
            assert_eq!(saved["dispatch_receipt"]["state"], "indeterminate");
            assert_eq!(drain_at(&rt, later).await.invoked, 0);
        } else {
            assert_eq!(summary.advanced, 1);
            assert_eq!(
                saved["trigger_at"],
                (later + Duration::days(1) - Duration::minutes(1)).to_rfc3339()
            );
            assert_eq!(saved["dispatch_receipt"]["state"], "failed");
        }
        assert_eq!(effects(&rt).await, 0);
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn active_v2_receipt_rejects_wrong_source_selection_bound_or_identity() {
    let (_dir, rt) = fixture().await;
    let now = clock();
    let source = (now - Duration::days(3) - Duration::minutes(10)).to_rfc3339();
    let id = create_event(&rt, catch_up_properties(&source, Some("daily"), 3600)).await;
    let claim = claim_catch_up(&rt, id, now).await;
    let saved = properties(&rt, id).await;
    let receipt = saved["dispatch_receipt"].clone();
    assert!(validate_dispatch_receipt(id, claim.firing_at, &saved, receipt.clone()).is_ok());
    for (field, invalid) in [
        ("source_trigger_at", json!(source.replace("+00:00", "Z"))),
        ("scheduled_for", json!(source)),
        (
            "decision_at",
            json!((at(&source) - Duration::seconds(1)).timestamp_micros()),
        ),
        ("effective_max_lateness", json!(0)),
        ("effective_max_lateness", json!(3601)),
        (
            "occurrence_id",
            json!(dispatch_occurrence_id(id, at(&source))),
        ),
        ("version", json!(1)),
    ] {
        let mut corrupted = receipt.clone();
        corrupted[field] = invalid;
        assert!(
            validate_dispatch_receipt(id, claim.firing_at, &saved, corrupted).is_err(),
            "{field}"
        );
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn catch_up_preserves_cancel_reschedule_and_finalize_fences() {
    use race_seam::{PauseGate, PausePoint, PAUSE_GATE};
    for scenario in [
        "cancel-before-claim",
        "reschedule-before-claim",
        "reschedule-before-finalize",
    ] {
        let (_dir, rt) = fixture().await;
        let now = clock();
        let source = (now - Duration::days(2) - Duration::minutes(10)).to_rfc3339();
        let id = create_event(&rt, catch_up_properties(&source, Some("daily"), 3600)).await;
        let server = KhiveMcpServer::new(rt.clone()).expect("server");
        let gate = PauseGate {
            at: if scenario == "reschedule-before-finalize" {
                PausePoint::BeforeFinalizeRead
            } else {
                PausePoint::BeforeClaim
            },
            reached: std::sync::Arc::new(tokio::sync::Barrier::new(2)),
            release: std::sync::Arc::new(tokio::sync::Barrier::new(2)),
        };
        let drain = MISFIRE_NOW.scope(
            now,
            PAUSE_GATE.scope(gate.clone(), run_pending_events_on(&rt, &server, false)),
        );
        let write = async {
            gate.reached.wait().await;
            let mut value = properties(&rt, id).await;
            if scenario == "cancel-before-claim" {
                value["status"] = json!("cancelled");
            } else {
                // Same instant, different spelling: even this must lose the
                // raw-source CAS/active-receipt fence, not rewrite L's identity.
                value["trigger_at"] = json!(source.replace("+00:00", "Z"));
            }
            replace_properties(&rt, id, value).await;
            gate.release.wait().await;
        };
        let (summary, ()) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            tokio::join!(drain, write)
        })
        .await
        .expect("race handshake");
        let summary = summary.expect("drain result");
        if scenario == "reschedule-before-finalize" {
            assert_eq!(summary.invoked, 1);
            assert_eq!(summary.finalized, 0);
            assert_eq!(effects(&rt).await, 1);
            let recovered =
                reclaim_stale_firing_events(&rt, Utc::now().timestamp_micros() + 1_000_000)
                    .await
                    .unwrap();
            assert_eq!(recovered.indeterminate, 1);
            assert_eq!(properties(&rt, id).await["status"], "failed");
            assert_eq!(drain_at(&rt, now).await.invoked, 0);
            assert_eq!(effects(&rt).await, 1);
        } else {
            assert_eq!(summary.invoked, 0);
            assert_eq!(summary.skipped_race, 1);
            assert_eq!(effects(&rt).await, 0);
        }
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn catch_up_does_not_bypass_immutable_creator_provenance() {
    let (_dir, rt) = fixture().await;
    let now = clock();
    let source = (now - Duration::days(2) - Duration::minutes(10)).to_rfc3339();
    let token = rt.authorize(Namespace::local()).expect("authorize");
    let note = rt
        .create_note(
            &token,
            "scheduled_event",
            None,
            "no creator proof",
            None,
            Some(catch_up_properties(&source, Some("daily"), 3600)),
            vec![],
        )
        .await
        .expect("legacy fixture without provenance");
    let summary = drain_at(&rt, now).await;
    assert_eq!(summary.invoked, 0);
    assert_eq!(effects(&rt).await, 0);
    let saved = properties(&rt, note.id).await;
    assert_eq!(saved["status"], "failed");
    assert_eq!(saved["dispatch_receipt"]["state"], "not_invoked");
    assert!(saved["dispatch_receipt"]["error"]
        .as_str()
        .unwrap()
        .contains("missing immutable creator provenance"));
}
