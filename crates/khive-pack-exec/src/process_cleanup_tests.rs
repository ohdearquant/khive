use super::Receipt;
use crate::process_cleanup::{
    CleanupCertification, CleanupObservation, CleanupScope, ProcessCleanup, TreeQuiescence,
};
use crate::tree::Change;
use serde_json::{json, Value};

fn collected_receipt() -> Receipt {
    Receipt {
        id: "synthetic-collected-run".into(),
        actor: "cleanup-reader".into(),
        tool: "fixture-tool".into(),
        argv: vec!["fixture-tool".into()],
        tree_in: "1".repeat(64),
        tree_out: Some("2".repeat(64)),
        exit_code: Some(0),
        exit_signal: None,
        limiting_resource: None,
        timed_out: false,
        process_cleanup: ProcessCleanup::seatbelt_unobserved(),
        tree_quiescence: TreeQuiescence::Unverified,
        denied: false,
        success: true,
        reason: None,
        refusal_code: "exec_refused",
        refusal_detail: Value::Null,
        decision: Some(json!({"decision": "allow"})),
        stdout_ref: Some("3".repeat(64)),
        stderr_ref: Some("4".repeat(64)),
        effective_max_output_bytes: 128,
        stdout_produced: 9,
        stderr_produced: 4,
        stdout_retained: 9,
        stderr_retained: 4,
        stdout_capture: "complete",
        stderr_capture: "complete",
        tree_capture: "complete",
        tree_capture_detail: None,
        changed: vec![Change {
            path: "result.txt".into(),
            op: "added",
            content_ref: Some("5".repeat(64)),
            base_ref: None,
        }],
        undeclared: vec![],
        skipped: vec![],
        cwd: ".".into(),
        env_keys: vec!["HOME".into()],
        session_id: None,
        seq: None,
        sandbox: None,
        profile_ref: None,
        limits: json!({"requested": {}, "enforced": {}}),
        pids: None,
        started_at: Some(1_000_000),
        finished_at: Some(1_001_000),
        #[cfg(unix)]
        owned_run_dir: None,
    }
}

#[test]
fn cleanup_vocabulary_round_trips_and_refuses_unknown_values() {
    assert_eq!(
        serde_json::to_value(CleanupScope::InitialGroup).unwrap(),
        json!("initial_group")
    );
    assert_eq!(
        serde_json::from_value::<CleanupScope>(json!("initial_group")).unwrap(),
        CleanupScope::InitialGroup
    );
    assert!(serde_json::from_value::<CleanupScope>(json!("all_descendants")).is_err());

    for (value, spelling) in [
        (CleanupObservation::NotAttempted, "not_attempted"),
        (CleanupObservation::Complete, "complete"),
        (CleanupObservation::Incomplete, "incomplete"),
    ] {
        assert_eq!(serde_json::to_value(value).unwrap(), json!(spelling));
        assert_eq!(
            serde_json::from_value::<CleanupObservation>(json!(spelling)).unwrap(),
            value
        );
    }
    assert!(serde_json::from_value::<CleanupObservation>(json!("partial")).is_err());

    for (value, spelling) in [
        (CleanupCertification::Unverified, "unverified"),
        (CleanupCertification::CertifiedNone, "certified_none"),
    ] {
        assert_eq!(serde_json::to_value(value).unwrap(), json!(spelling));
        assert_eq!(
            serde_json::from_value::<CleanupCertification>(json!(spelling)).unwrap(),
            value
        );
    }
    assert!(serde_json::from_value::<CleanupCertification>(json!("none")).is_err());

    for (value, spelling) in [
        (TreeQuiescence::Unverified, "unverified"),
        (TreeQuiescence::Certified, "certified"),
    ] {
        assert_eq!(serde_json::to_value(value).unwrap(), json!(spelling));
        assert_eq!(
            serde_json::from_value::<TreeQuiescence>(json!(spelling)).unwrap(),
            value
        );
    }
    assert!(serde_json::from_value::<TreeQuiescence>(json!("quiescent")).is_err());

    for observation in [
        CleanupObservation::NotAttempted,
        CleanupObservation::Complete,
        CleanupObservation::Incomplete,
    ] {
        for seen_alive in [false, true] {
            let mut cleanup = ProcessCleanup::seatbelt_unobserved();
            cleanup.observation = observation;
            cleanup.seen_alive = seen_alive;
            let value = serde_json::to_value(&cleanup).unwrap();
            assert_eq!(
                serde_json::from_value::<ProcessCleanup>(value).unwrap(),
                cleanup
            );
        }
    }
    let original = serde_json::to_value(ProcessCleanup::seatbelt_unobserved()).unwrap();
    for (field, invalid) in [
        ("scope", json!("all_descendants")),
        ("observation", json!("partial")),
        ("certification", json!("none")),
        ("seen_alive", json!("true")),
    ] {
        let mut value = original.clone();
        value[field] = invalid;
        assert!(serde_json::from_value::<ProcessCleanup>(value).is_err());
    }
    let mut extra = original.clone();
    extra["whole_tree_dead"] = json!(true);
    assert!(serde_json::from_value::<ProcessCleanup>(extra).is_err());
    let mut missing = original;
    let _ = missing.as_object_mut().unwrap().remove("seen_alive");
    assert!(serde_json::from_value::<ProcessCleanup>(missing).is_err());
}

#[test]
fn incomplete_observation_preserves_positive_sighting() {
    let mut receipt = collected_receipt();
    receipt.process_cleanup.observation = CleanupObservation::Complete;
    receipt.process_cleanup.seen_alive = true;
    receipt.process_cleanup.observation = CleanupObservation::Incomplete;
    receipt.process_cleanup.detail = "Observation stopped after a positive sighting.".into();

    let value = receipt.to_json();
    assert_eq!(value["process_cleanup"]["observation"], "incomplete");
    assert_eq!(value["process_cleanup"]["seen_alive"], true);
    assert_eq!(value["process_cleanup"]["certification"], "unverified");
    let decoded: ProcessCleanup = serde_json::from_value(value["process_cleanup"].clone()).unwrap();
    assert!(decoded.seen_alive);
    assert_eq!(decoded, receipt.process_cleanup);
}

#[test]
fn provisional_publication_keeps_artifacts_and_execution_success_for_all_observations() {
    let observations = [
        (CleanupObservation::NotAttempted, false),
        (CleanupObservation::Complete, false),
        (CleanupObservation::Incomplete, false),
        (CleanupObservation::Incomplete, true),
        (CleanupObservation::Complete, true),
    ];
    for outcome in [
        "collected",
        "child_failed",
        "deadline",
        "capture_failed",
        "root_degraded",
    ] {
        let mut receipt = collected_receipt();
        match outcome {
            "collected" => {}
            "child_failed" => {
                receipt.exit_code = Some(7);
                receipt.success = false;
            }
            "deadline" => {
                receipt.timed_out = true;
                receipt.success = false;
                receipt.reason = Some("output collection reached the run deadline".into());
            }
            "capture_failed" | "root_degraded" => {
                receipt.success = false;
                receipt.tree_out = None;
                receipt.changed.clear();
                receipt.tree_capture = if outcome == "capture_failed" {
                    "failed"
                } else {
                    "degraded"
                };
                receipt.reason = Some("actual capture did not produce an output tree".into());
                if outcome == "root_degraded" {
                    receipt.tree_capture_detail = Some("root_missing".into());
                }
            }
            _ => unreachable!(),
        }
        let before = receipt.to_json();
        assert_eq!(before["success"], outcome == "collected");
        assert_eq!(
            before["tree_out"].is_string(),
            matches!(outcome, "collected" | "child_failed" | "deadline")
        );
        for (observation, seen_alive) in observations {
            receipt.process_cleanup.observation = observation;
            receipt.process_cleanup.seen_alive = seen_alive;
            let value = receipt.to_json();
            assert_eq!(value["tree_quiescence"], "unverified");
            assert_eq!(value["process_cleanup"]["seen_alive"], seen_alive);
            for field in [
                "tree_out",
                "changed",
                "success",
                "exit_code",
                "exit_signal",
                "timed_out",
                "stdout_ref",
                "stderr_ref",
                "stdout_produced_bytes",
                "stderr_produced_bytes",
                "stdout_retained_bytes",
                "stderr_retained_bytes",
                "stdout_capture",
                "stderr_capture",
                "tree_capture",
                "tree_capture_detail",
                "reason",
                "undeclared_changes",
                "skipped",
            ] {
                assert_eq!(
                    value[field], before[field],
                    "{outcome}, {observation:?}, seen_alive={seen_alive}, field={field}"
                );
            }
        }
    }
}

#[test]
fn seatbelt_producer_keeps_certification_unverified() {
    let cleanup = ProcessCleanup::seatbelt_unobserved();
    assert_eq!(cleanup.scope, CleanupScope::InitialGroup);
    assert_eq!(cleanup.observation, CleanupObservation::NotAttempted);
    assert!(!cleanup.seen_alive);
    assert_eq!(cleanup.certification, CleanupCertification::Unverified);
    assert_eq!(
        cleanup.detail,
        "Detached descendant termination is not certified on this backend."
    );
    let value = serde_json::to_value(cleanup).unwrap();
    assert_eq!(value["certification"], "unverified");
    assert_ne!(value["certification"], "certified_none");
}
