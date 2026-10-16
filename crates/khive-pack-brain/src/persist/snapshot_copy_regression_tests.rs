use super::*;
use khive_brain_core::brain_state::{AdapterRecord, RouterStateBlob};
use khive_runtime::{KhiveRuntime, Namespace};

#[tokio::test]
async fn snapshot_encoding_preserves_atomic_rollback_and_opaque_state() {
    let rt = KhiveRuntime::memory().unwrap();
    let token = rt.authorize(Namespace::local()).unwrap();
    let sql = rt.sql();
    let tracker = Mutex::new(PersistenceTracker::new());
    let mut initial = BrainState::new(16);
    initial.router_state.insert(
        "test-router".into(),
        RouterStateBlob {
            schema_version: 3,
            gate_bytes: vec![47; 4096],
        },
    );
    initial.adapter_set.insert(
        "test-router".into(),
        vec![AdapterRecord {
            adapter_id: "adapter-original".into(),
            slot: 2,
            content_hash: "original-hash".into(),
        }],
    );
    let state = Mutex::new(initial);
    let before = serde_json::to_value(state.lock().unwrap().to_snapshot()).unwrap();
    sql.writer().await.unwrap().execute_script(
            "CREATE TRIGGER refuse_snapshot_1441 BEFORE INSERT ON brain_profile_snapshots BEGIN SELECT RAISE(ABORT, '1441 injected snapshot failure'); END;".into()
        ).await.unwrap();

    for succeeds in [false, true] {
        let result = persist_brain_state_mutation(
            sql.as_ref(),
            &token,
            &tracker,
            &state,
            BrainMutationEvent {
                profile_id: "balanced-recall-v1".into(),
                event_kind: "brain.copy_probe".into(),
                payload: serde_json::json!({"opaque_state": true}),
            },
            16,
            |proposed| {
                proposed.balanced_recall.total_events += 1;
                proposed
                    .router_state
                    .get_mut("test-router")
                    .unwrap()
                    .gate_bytes[0] = 91;
                proposed.adapter_set.get_mut("test-router").unwrap()[0].content_hash =
                    "new-hash".into();
                Ok(())
            },
        )
        .await;
        if succeeds {
            result.unwrap();
            let (persisted, _) = load_latest_snapshot(sql.as_ref(), "local", 16)
                .await
                .unwrap()
                .unwrap();
            let live = state.lock().unwrap().to_snapshot();
            assert_eq!(live.balanced_recall.total_events, 1);
            assert_eq!(persisted.router_state["test-router"].gate_bytes[0], 91);
            assert_eq!(
                persisted.adapter_set["test-router"][0].content_hash,
                "new-hash"
            );
            assert_eq!(
                serde_json::to_value(persisted).unwrap(),
                serde_json::to_value(live).unwrap()
            );
        } else {
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("1441 injected snapshot failure"));
            assert_eq!(
                serde_json::to_value(state.lock().unwrap().to_snapshot()).unwrap(),
                before
            );
            assert!(!tracker.lock().unwrap().is_loaded("local"));
            assert!(load_latest_snapshot(sql.as_ref(), "local", 16)
                .await
                .unwrap()
                .is_none());
        }
        let row = sql.reader().await.unwrap().query_row(SqlStatement {
                sql: "SELECT COUNT(*) AS n FROM brain_event_log WHERE namespace = 'local' AND event_kind = 'brain.copy_probe'".into(),
                params: vec![], label: None,
            }).await.unwrap().unwrap();
        match row.get("n") {
            Some(SqlValue::Integer(n)) => assert_eq!(*n, i64::from(succeeds)),
            other => panic!("expected an integer count, got {other:?}"),
        }
        if !succeeds {
            sql.writer()
                .await
                .unwrap()
                .execute_script("DROP TRIGGER refuse_snapshot_1441;".into())
                .await
                .unwrap();
        }
    }
}

#[tokio::test]
async fn feedback_snapshot_failure_rolls_back_events_claim_and_opaque_state() {
    use crate::fold_gate::FeedbackGateMode;
    use khive_types::{EventKind, SubstrateKind};

    let rt = KhiveRuntime::memory().unwrap();
    let token = rt.authorize(Namespace::local()).unwrap();
    let sql = rt.sql();
    let tracker = Mutex::new(PersistenceTracker::new());
    let mut initial = BrainState::new(16);
    initial.router_state.insert(
        "feedback-router".into(),
        RouterStateBlob {
            schema_version: 3,
            gate_bytes: vec![47; 1024],
        },
    );
    let initial_alpha = initial.balanced_recall.salience.alpha();
    let state = Mutex::new(initial);
    let before = serde_json::to_value(state.lock().unwrap().to_snapshot()).unwrap();
    let target = uuid::Uuid::new_v4();
    let event = Event::new(
        "local",
        "brain.feedback",
        EventKind::FeedbackExplicit,
        SubstrateKind::Note,
        "brain",
    )
    .with_target(target)
    .with_payload(serde_json::json!({
        "signal": "implicit_positive",
        "served_by_profile_id": "balanced-recall-v1",
        "scorer_run_id": "snapshot-scorer",
        "serve_ledger_id": "snapshot-ledger"
    }));
    sql.writer().await.unwrap().execute_script(
            "CREATE TRIGGER refuse_feedback_snapshot BEFORE INSERT ON brain_profile_snapshots BEGIN SELECT RAISE(ABORT, 'injected feedback snapshot failure'); END;".into()
        ).await.unwrap();

    // Reuse the event and claim identities: an orphan from the first
    // attempt must not turn the valid retry into a conflict or a no-op.
    for succeeds in [false, true] {
        let result = persist_feedback_state_mutation(
            sql.as_ref(),
            &token,
            &tracker,
            &state,
            "balanced-recall-v1".into(),
            FeedbackEventWrite::Gated {
                event: event.clone(),
                target_id: target.to_string(),
                gate_mode: FeedbackGateMode::Nominal(0.1),
                gate_now_us: 1_700_000_000_000_000,
                dedup_key: Some(("snapshot-scorer".into(), "snapshot-ledger".into())),
                grade: None,
            },
            16,
            |proposed, signal| {
                proposed.balanced_recall.apply_signal(signal);
                proposed
                    .router_state
                    .get_mut("feedback-router")
                    .unwrap()
                    .gate_bytes[0] = 91;
            },
        )
        .await;
        let committed = if succeeds {
            let committed = result.unwrap().expect("retry must not be deduplicated");
            assert_eq!(committed.id, event.id);
            assert_eq!(committed.payload["gate"]["effective_weight"], 0.1);
            let (persisted, _) = load_latest_snapshot(sql.as_ref(), "local", 16)
                .await
                .unwrap()
                .unwrap();
            let live = state.lock().unwrap().to_snapshot();
            assert_eq!(live.balanced_recall.total_events, 1);
            assert!((live.balanced_recall.salience.alpha() - initial_alpha - 0.1).abs() < 1e-9);
            assert_eq!(persisted.router_state["feedback-router"].gate_bytes[0], 91);
            assert_eq!(
                serde_json::to_value(persisted).unwrap(),
                serde_json::to_value(live).unwrap()
            );
            Some(committed)
        } else {
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("injected feedback snapshot failure"));
            assert_eq!(
                serde_json::to_value(state.lock().unwrap().to_snapshot()).unwrap(),
                before
            );
            assert!(load_latest_snapshot(sql.as_ref(), "local", 16)
                .await
                .unwrap()
                .is_none());
            None
        };
        assert_eq!(tracker.lock().unwrap().is_loaded("local"), succeeds);
        assert_eq!(
            rt.events(&token)
                .unwrap()
                .get_event(event.id)
                .await
                .unwrap(),
            committed
        );
        let row = sql.reader().await.unwrap().query_row(SqlStatement {
                sql: "SELECT (SELECT COUNT(*) FROM brain_event_log WHERE namespace = 'local' AND event_kind = 'brain.feedback') AS private_events, (SELECT COUNT(*) FROM brain_scorer_dedup) AS claims, (SELECT COUNT(*) FROM brain_implicit_mass) AS mass_rows".into(),
                params: vec![], label: None,
            }).await.unwrap().unwrap();
        for field in ["private_events", "claims", "mass_rows"] {
            match row.get(field) {
                Some(SqlValue::Integer(n)) => {
                    assert_eq!(*n, i64::from(succeeds), "{field}")
                }
                other => panic!("{field}: expected an integer, got {other:?}"),
            }
        }
        if !succeeds {
            sql.writer()
                .await
                .unwrap()
                .execute_script("DROP TRIGGER refuse_feedback_snapshot;".into())
                .await
                .unwrap();
        }
    }
}
