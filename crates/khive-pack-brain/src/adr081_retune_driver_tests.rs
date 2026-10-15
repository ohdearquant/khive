use super::*;

#[tokio::test]
async fn implicit_weight_is_0_1_not_0_5() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let target = create_test_entity(&rt, &token).await;

    let sal_beta_before = pack.snapshot().balanced_recall.salience.beta();

    pack.dispatch(
        "brain.feedback",
        json!({"target_id": target, "signal": "implicit_negative"}),
        &registry,
        &token,
    )
    .await
    .unwrap();

    let sal_beta_after = pack.snapshot().balanced_recall.salience.beta();
    assert!(
        (sal_beta_after - (sal_beta_before + 0.1)).abs() < 1e-9,
        "ADR-081 §1: a single fresh-target implicit_negative must fold at 0.1, \
             not 0.5; before={sal_beta_before}, after={sal_beta_after}"
    );
}

#[tokio::test]
async fn scorer_params_together_or_rejected_scorer_run_id_alone() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let target = create_test_entity(&rt, &token).await;

    let err = pack
        .dispatch(
            "brain.feedback",
            json!({
                "target_id": target,
                "signal": "implicit_positive",
                "scorer_run_id": "run-1",
            }),
            &registry,
            &token,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, RuntimeError::InvalidInput(_)),
        "scorer_run_id alone must be rejected as invalid input, got {err:?}"
    );
}

#[tokio::test]
async fn scorer_params_together_or_rejected_serve_ledger_id_alone() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let target = create_test_entity(&rt, &token).await;

    let err = pack
        .dispatch(
            "brain.feedback",
            json!({
                "target_id": target,
                "signal": "implicit_positive",
                "serve_ledger_id": "row-1",
            }),
            &registry,
            &token,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, RuntimeError::InvalidInput(_)),
        "serve_ledger_id alone must be rejected as invalid input, got {err:?}"
    );
}

#[tokio::test]
async fn scorer_params_absent_both_is_unchanged_behavior() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let target = create_test_entity(&rt, &token).await;

    let result = pack
        .dispatch(
            "brain.feedback",
            json!({"target_id": target, "signal": "implicit_positive"}),
            &registry,
            &token,
        )
        .await
        .expect("ordinary feedback with neither scorer param must succeed");
    assert_eq!(result["emitted"], json!(true));
}

/// ADR-081 §2: fold gate saturation. 16 implicit_negative events against the
/// same fresh target (weight 0.1 each, no decay since they land at the same
/// instant) must fold the first 15 at full weight (summing to the cap, 1.5)
/// and clamp the 16th to zero — salience.beta() must not move past the cap's
/// contribution, and the raw event log must still record all 16 (audit
/// preserved, never dropped).
#[tokio::test]
async fn fold_gate_clamps_after_cap_history_preserved() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let target = create_test_entity(&rt, &token).await;

    let sal_beta_before = pack.snapshot().balanced_recall.salience.beta();

    for _ in 0..16 {
        pack.dispatch(
            "brain.feedback",
            json!({"target_id": target, "signal": "implicit_negative"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    }

    let sal_beta_after = pack.snapshot().balanced_recall.salience.beta();
    assert!(
        (sal_beta_after - (sal_beta_before + 1.5)).abs() < 1e-6,
        "16 events at weight 0.1 must fold at most 1.5 total into salience.beta(); \
             before={sal_beta_before}, after={sal_beta_after}"
    );

    // Audit preserved: all 16 raw events land in brain_event_log regardless
    // of the gate's fold decision (data-vs-view — never dropped).
    let events = crate::persist::load_events_since(rt.sql().as_ref(), "local", 0)
        .await
        .expect("load events");
    let feedback_count = events
        .events
        .iter()
        .filter(|e| e.verb == "brain.feedback")
        .count();
    assert_eq!(
        feedback_count, 16,
        "all 16 implicit events must remain in the audit log, clamped or not"
    );
}

#[tokio::test]
async fn fold_gate_exempts_explicit_and_correction_signals() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let target = create_test_entity(&rt, &token).await;

    // 20 explicit_positive events (weight 1.5 each) must all fold at full
    // weight — the clamp applies only to implicit signals (ADR-081 §2).
    for _ in 0..20 {
        pack.dispatch(
            "brain.feedback",
            json!({"target_id": target, "signal": "explicit_positive"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    }

    let ep = pack
        .snapshot()
        .balanced_recall
        .entity_posteriors
        .get(&uuid::Uuid::parse_str(&target).unwrap())
        .cloned()
        .expect("entity posterior must exist");
    assert!(
        (ep.alpha() - (1.0 + 20.0 * 1.5)).abs() < 1e-6,
        "20 explicit_positive events must each fold at 1.5, unclamped; got alpha={}",
        ep.alpha()
    );
}

/// Half-life decay arithmetic, exercised through the live handler: two
/// implicit events separated by exactly one half-life (7 days) should NOT
/// clamp even when their un-decayed sum would exceed the cap, because the
/// first event's contribution has decayed to half by the time the second
/// lands. This pins `fold_gate::decayed_mass`'s formula end to end (not
/// just the pure-function unit tests in fold_gate.rs) by manipulating the
/// mass row's `last_event_at` directly between two live dispatches.
#[tokio::test]
async fn fold_gate_half_life_decay_prevents_premature_clamp() {
    use khive_storage::types::{SqlStatement, SqlValue};

    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let target = create_test_entity(&rt, &token).await;

    // First implicit event: mass becomes 0.1 at "now".
    pack.dispatch(
        "brain.feedback",
        json!({"target_id": target, "signal": "implicit_negative"}),
        &registry,
        &token,
    )
    .await
    .unwrap();

    // Rewrite last_event_at to 7 days in the past, and bump mass to just
    // under the cap (1.45), simulating 14 prior events that have not yet
    // decayed. Without decay, 1.45 + 0.1 = 1.55 > 1.5 would clamp. With one
    // half-life of decay, 1.45 decays to 0.725, so 0.725 + 0.1 <= 1.5 passes.
    let half_life_us = crate::fold_gate::IMPLICIT_MASS_HALF_LIFE_US as i64;
    let now_us = chrono::Utc::now().timestamp_micros();
    let mut writer = rt.sql().writer().await.unwrap();
    writer
        .execute(SqlStatement {
            sql: "UPDATE brain_implicit_mass SET mass = ?1, last_event_at = ?2 \
                      WHERE namespace = 'local'"
                .into(),
            params: vec![
                SqlValue::Float(1.45),
                SqlValue::Integer(now_us - half_life_us),
            ],
            label: None,
        })
        .await
        .unwrap();

    let sal_beta_before = pack.snapshot().balanced_recall.salience.beta();
    pack.dispatch(
        "brain.feedback",
        json!({"target_id": target, "signal": "implicit_negative"}),
        &registry,
        &token,
    )
    .await
    .unwrap();
    let sal_beta_after = pack.snapshot().balanced_recall.salience.beta();

    assert!(
        (sal_beta_after - (sal_beta_before + 0.1)).abs() < 1e-6,
        "decayed mass (0.725) + 0.1 must pass the cap; event must fold at full \
             weight, not clamp. before={sal_beta_before}, after={sal_beta_after}"
    );
}

#[tokio::test]
async fn serve_ledger_dedup_second_emit_is_no_op() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let target = create_test_entity(&rt, &token).await;

    crate::serve_ledger::record_serve(
        rt.sql().as_ref(),
        "ledger-row-1",
        "local",
        "recall",
        Some("balanced-recall-v1"),
        None,
        None,
        &target,
        "class-1",
        "raw query",
        1_000,
        None,
    )
    .await
    .expect("record_serve");

    let first_usage = khive_storage::usage::UsageContext::new();
    let first = khive_storage::usage::scope(
        first_usage.clone(),
        pack.dispatch(
            "brain.feedback",
            json!({
                "target_id": target,
                "signal": "implicit_positive",
                "scorer_run_id": "scorer-run-1",
                "serve_ledger_id": "ledger-row-1",
            }),
            &registry,
            &token,
        ),
    )
    .await
    .expect("first emit must succeed");
    assert_eq!(first["emitted"], json!(true));
    assert_eq!(first_usage.snapshot()["event_rows"], json!(1u64));

    let sal_beta_after_first = pack.snapshot().balanced_recall.salience.beta();

    let second_usage = khive_storage::usage::UsageContext::new();
    let second = khive_storage::usage::scope(
        second_usage.clone(),
        pack.dispatch(
            "brain.feedback",
            json!({
                "target_id": target,
                "signal": "implicit_positive",
                "scorer_run_id": "scorer-run-1",
                "serve_ledger_id": "ledger-row-1",
            }),
            &registry,
            &token,
        ),
    )
    .await
    .expect("duplicate emit must be a no-op, not an error");
    assert_eq!(
        second["deduped"],
        json!(true),
        "second emit with the same (scorer_run_id, serve_ledger_id) must be deduped"
    );
    assert_eq!(second["emitted"], json!(false));
    assert!(
        second_usage.snapshot().get("event_rows").is_none(),
        "deduped feedback must not count an event row"
    );

    let sal_beta_after_second = pack.snapshot().balanced_recall.salience.beta();
    assert_eq!(
        sal_beta_after_first, sal_beta_after_second,
        "deduped emit must not fold a second time"
    );
}

#[tokio::test]
async fn failed_ledger_grade_rolls_back_scorer_claim_and_retries() {
    for (case, serving_profile) in [
        ("profile", Some("balanced-recall-v1")),
        ("unattributed", None),
    ] {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local()).unwrap();
        let target = create_test_entity(&rt, &token).await;
        let ledger_id = format!("ledger-grade-{case}");
        let scorer_run_id = format!("scorer-grade-{case}");
        crate::serve_ledger::record_serve(
            rt.sql().as_ref(),
            &ledger_id,
            "local",
            "recall",
            serving_profile,
            None,
            None,
            &target,
            "grade-retry-class",
            "grade retry query",
            1_000,
            None,
        )
        .await
        .expect("record serve");

        rt.sql()
            .writer()
            .await
            .expect("writer")
            .execute_script(
                "CREATE TRIGGER refuse_brain_grade BEFORE UPDATE OF grade ON brain_serve_ledger \
                     BEGIN SELECT RAISE(ABORT, 'injected grade failure'); END;"
                    .into(),
            )
            .await
            .expect("install grade failure trigger");

        let request = json!({
            "target_id": target,
            "signal": "implicit_positive",
            "scorer_run_id": scorer_run_id,
            "serve_ledger_id": ledger_id,
        });
        let failed = pack
            .dispatch("brain.feedback", request.clone(), &registry, &token)
            .await;
        assert!(
            failed.is_err(),
            "{case}: grade failure must refuse feedback"
        );
        let row = crate::serve_ledger::get_serve_row(rt.sql().as_ref(), &ledger_id)
            .await
            .expect("read serve row")
            .expect("serve row exists");
        assert!(row.grade.is_none(), "{case}: grade must remain unset");
        assert!(
            row.scorer_run_id.is_none(),
            "{case}: scorer must remain unset"
        );

        rt.sql()
            .writer()
            .await
            .expect("writer")
            .execute_script("DROP TRIGGER refuse_brain_grade;".into())
            .await
            .expect("remove grade failure trigger");
        let retry = pack
            .dispatch("brain.feedback", request.clone(), &registry, &token)
            .await
            .expect("same scorer run must be repairable");
        assert_eq!(retry["emitted"], true, "{case}: retry must emit");
        let row = crate::serve_ledger::get_serve_row(rt.sql().as_ref(), &ledger_id)
            .await
            .expect("read repaired row")
            .expect("serve row exists");
        assert_eq!(row.grade.as_deref(), Some("implicit_positive"));
        assert_eq!(row.scorer_run_id.as_deref(), Some(scorer_run_id.as_str()));
        let duplicate = pack
            .dispatch("brain.feedback", request, &registry, &token)
            .await
            .expect("committed scorer run should dedup");
        assert_eq!(duplicate["deduped"], true, "{case}: third call must dedup");
    }
}

#[tokio::test]
async fn serve_ledger_accounting_profile_id_prefers_served_by() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let target = create_test_entity(&rt, &token).await;

    crate::serve_ledger::record_serve(
        rt.sql().as_ref(),
        "ledger-row-2",
        "local",
        "recall",
        Some("balanced-recall-v1"),
        Some("some-other-profile"),
        Some(500),
        &target,
        "class-2",
        "raw query",
        1_000,
        None,
    )
    .await
    .expect("record_serve");

    let result = pack
        .dispatch(
            "brain.feedback",
            json!({
                "target_id": target,
                "signal": "implicit_positive",
                "scorer_run_id": "scorer-run-2",
                "serve_ledger_id": "ledger-row-2",
            }),
            &registry,
            &token,
        )
        .await
        .expect("resolved accounting profile must fold normally");
    assert_eq!(result["emitted"], json!(true));

    let row = crate::serve_ledger::get_serve_row(rt.sql().as_ref(), "ledger-row-2")
        .await
        .unwrap()
        .expect("row must exist");
    assert_eq!(row.grade.as_deref(), Some("implicit_positive"));
    assert_eq!(row.scorer_run_id.as_deref(), Some("scorer-run-2"));
}

#[tokio::test]
async fn serve_ledger_accounting_profile_overrides_default_and_rejects_misstatement() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let target = create_test_entity(&rt, &token).await;

    pack.dispatch(
        "brain.create_profile",
        json!({"name": "ledger-profile-v1", "consumer_kind": "recall"}),
        &registry,
        &token,
    )
    .await
    .unwrap();
    pack.dispatch(
        "brain.activate",
        json!({"profile_id": "ledger-profile-v1"}),
        &registry,
        &token,
    )
    .await
    .unwrap();

    crate::serve_ledger::record_serve(
        rt.sql().as_ref(),
        "ledger-profile-row",
        "local",
        "recall",
        Some("ledger-profile-v1"),
        Some("balanced-recall-v1"),
        Some(500),
        &target,
        "class-profile",
        "raw query",
        1_000,
        None,
    )
    .await
    .unwrap();

    let before = pack.snapshot();
    let mismatch = pack
        .dispatch(
            "brain.feedback",
            json!({
                "target_id": target,
                "signal": "implicit_positive",
                "served_by_profile_id": "balanced-recall-v1",
                "scorer_run_id": "ledger-profile-run",
                "serve_ledger_id": "ledger-profile-row",
            }),
            &registry,
            &token,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(mismatch, RuntimeError::InvalidInput(message) if message.contains("accounting_profile_id"))
    );
    assert_eq!(pack.snapshot().balanced_recall, before.balanced_recall);

    // No caller profile: the default resolver would choose balanced-recall-v1,
    // while the row says ledger-profile-v1 actually served this target.
    let result = pack
        .dispatch(
            "brain.feedback",
            json!({
                "target_id": target,
                "signal": "implicit_positive",
                "scorer_run_id": "ledger-profile-run",
                "serve_ledger_id": "ledger-profile-row",
            }),
            &registry,
            &token,
        )
        .await
        .unwrap();
    assert_eq!(result["served_by_profile_id"], "ledger-profile-v1");
    assert_eq!(result["serve_attribution"], "profile");

    let after = pack.snapshot();
    assert_eq!(
        after.profile_states["ledger-profile-v1"].total_events,
        before.profile_states["ledger-profile-v1"].total_events + 1,
    );
    assert_eq!(
        after.balanced_recall.total_events, before.balanced_recall.total_events,
        "the caller default must not receive the row's feedback"
    );
}

#[tokio::test]
async fn serve_ledger_target_mismatch_is_rejected_before_and_after_grade() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let served_target = create_test_entity(&rt, &token).await;
    let other_target = create_test_entity(&rt, &token).await;

    crate::serve_ledger::record_serve(
        rt.sql().as_ref(),
        "ledger-target-row",
        "local",
        "recall",
        Some("balanced-recall-v1"),
        None,
        None,
        &served_target,
        "class-target",
        "raw query",
        1_000,
        None,
    )
    .await
    .unwrap();

    let params = |target: &str| {
        json!({
            "target_id": target,
            "signal": "implicit_positive",
            "scorer_run_id": "ledger-target-run",
            "serve_ledger_id": "ledger-target-row",
        })
    };
    let mismatch = pack
        .dispatch("brain.feedback", params(&other_target), &registry, &token)
        .await
        .unwrap_err();
    assert!(
        matches!(mismatch, RuntimeError::InvalidInput(message) if message.contains("target_id"))
    );
    assert!(
        crate::serve_ledger::get_serve_row(rt.sql().as_ref(), "ledger-target-row")
            .await
            .unwrap()
            .unwrap()
            .grade
            .is_none(),
        "a mismatched target must not grade the row"
    );

    let first = pack
        .dispatch("brain.feedback", params(&served_target), &registry, &token)
        .await
        .unwrap();
    assert_eq!(first["emitted"], true);
    let mismatched_replay = pack
        .dispatch("brain.feedback", params(&other_target), &registry, &token)
        .await
        .unwrap_err();
    assert!(
        matches!(mismatched_replay, RuntimeError::InvalidInput(message) if message.contains("target_id"))
    );
    let replay = pack
        .dispatch("brain.feedback", params(&served_target), &registry, &token)
        .await
        .unwrap();
    assert_eq!(replay["deduped"], true);
}

#[tokio::test]
async fn serve_ledger_resolve_checks_stored_namespace() {
    let (_pack, rt) = make_pack();
    let arm = Namespace::parse("bench-arm-a").unwrap();
    let token = rt.authorize(arm).unwrap();
    let target = create_test_entity(&rt, &token).await;

    crate::serve_ledger::record_serve(
        rt.sql().as_ref(),
        "ledger-arm-row",
        "bench-arm-a",
        "recall",
        None,
        None,
        None,
        &target,
        "class-arm",
        "raw query",
        1_000,
        Some("unattributed"),
    )
    .await
    .unwrap();

    let row = crate::serve_ledger::get_serve_row(rt.sql().as_ref(), "ledger-arm-row")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.namespace, "bench-arm-a");
    let wrong_namespace = crate::serve_ledger::resolve(
        rt.sql().as_ref(),
        "ledger-arm-row",
        "scorer-arm",
        "local",
        &target,
        None,
    )
    .await
    .err()
    .expect("a different namespace must be rejected");
    assert!(
        matches!(wrong_namespace, RuntimeError::InvalidInput(message) if message.contains("namespace"))
    );
    assert!(matches!(
        crate::serve_ledger::resolve(
            rt.sql().as_ref(),
            "ledger-arm-row",
            "scorer-arm",
            "bench-arm-a",
            &target,
            None,
        )
        .await
        .unwrap(),
        crate::serve_ledger::ServeLedgerResolution::Found { .. }
    ));
}

#[tokio::test]
async fn explicit_scorer_feedback_refuses_before_claiming_dedup_key() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let target = create_test_entity(&rt, &token).await;
    crate::serve_ledger::record_serve(
        rt.sql().as_ref(),
        "ledger-explicit-row",
        "local",
        "recall",
        Some("balanced-recall-v1"),
        None,
        None,
        &target,
        "class-explicit",
        "raw query",
        1_000,
        None,
    )
    .await
    .unwrap();

    let rejected = pack
        .dispatch(
            "brain.feedback",
            json!({
                "target_id": target,
                "signal": "useful",
                "scorer_run_id": "ledger-explicit-run",
                "serve_ledger_id": "ledger-explicit-row",
            }),
            &registry,
            &token,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(rejected, RuntimeError::InvalidInput(message) if message.contains("implicit_positive"))
    );
    let row = crate::serve_ledger::get_serve_row(rt.sql().as_ref(), "ledger-explicit-row")
        .await
        .unwrap()
        .unwrap();
    assert!(row.grade.is_none());

    let accepted = pack
        .dispatch(
            "brain.feedback",
            json!({
                "target_id": target,
                "signal": "implicit_positive",
                "scorer_run_id": "ledger-explicit-run",
                "serve_ledger_id": "ledger-explicit-row",
            }),
            &registry,
            &token,
        )
        .await
        .unwrap();
    assert_eq!(accepted["emitted"], true);
}

#[tokio::test]
async fn serve_ledger_unresolved_profile_forces_zero_weight_failsafe() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let target = create_test_entity(&rt, &token).await;

    // Neither served_by_profile_id nor resolved_profile_id set — the ledger
    // row has no resolvable accounting profile.
    crate::serve_ledger::record_serve(
        rt.sql().as_ref(),
        "ledger-row-3",
        "local",
        "recall",
        None,
        None,
        None,
        &target,
        "class-3",
        "raw query",
        1_000,
        None,
    )
    .await
    .expect("record_serve");

    let state_before = pack.snapshot().balanced_recall;

    let result = pack
        .dispatch(
            "brain.feedback",
            json!({
                "target_id": target,
                "signal": "implicit_positive",
                "scorer_run_id": "scorer-run-3",
                "serve_ledger_id": "ledger-row-3",
            }),
            &registry,
            &token,
        )
        .await
        .expect("unresolved profile must still succeed, just at zero weight");

    let state_after = pack.snapshot().balanced_recall;
    assert_eq!(
        state_after, state_before,
        "ADR-081 §4 fail-safe: unresolved accounting profile must not mutate any profile state"
    );
    assert_eq!(result["serve_attribution"], json!("unattributed"));
    assert_eq!(result["served_by_profile_id"], Value::Null);
}

/// ADR-081 amendment regression: a serve row stamped with the tri-state
/// `unattributed` marker (a recall whose bound profile record could not be
/// read) must force zero weight exactly like the legacy no-marker row
/// above — the marker must not accidentally unlock crediting a guessed
/// profile.
#[tokio::test]
async fn serve_ledger_stored_unattributed_marker_forces_zero_weight_failsafe() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let target = create_test_entity(&rt, &token).await;

    crate::serve_ledger::record_serve(
        rt.sql().as_ref(),
        "ledger-row-unattributed",
        "local",
        "recall",
        None,
        None,
        None,
        &target,
        "class-unattributed",
        "raw query",
        1_000,
        Some("unattributed"),
    )
    .await
    .expect("record_serve");

    let state_before = pack.snapshot().balanced_recall;

    let result = pack
        .dispatch(
            "brain.feedback",
            json!({
                "target_id": target,
                "signal": "implicit_positive",
                "scorer_run_id": "scorer-run-unattributed",
                "serve_ledger_id": "ledger-row-unattributed",
            }),
            &registry,
            &token,
        )
        .await
        .expect("stored-unattributed row must still succeed, just at zero weight");

    let state_after = pack.snapshot().balanced_recall;
    assert_eq!(
        state_after, state_before,
        "a stored unattributed marker must force zero weight, same as a legacy null row"
    );
    assert_eq!(result["serve_attribution"], json!("unattributed"));
    assert_eq!(result["served_by_profile_id"], Value::Null);
}

#[tokio::test]
async fn serve_ledger_null_accounting_does_not_accept_archived_explicit_profile() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    create_active_lifecycle_profile(&pack, &registry, &token).await;
    let target = create_test_entity(&rt, &token).await;
    pack.dispatch(
        "brain.deactivate",
        json!({"profile_id": LIFECYCLE_PROFILE_ID}),
        &registry,
        &token,
    )
    .await
    .unwrap();
    pack.dispatch(
        "brain.archive",
        json!({"profile_id": LIFECYCLE_PROFILE_ID}),
        &registry,
        &token,
    )
    .await
    .unwrap();

    crate::serve_ledger::record_serve(
        rt.sql().as_ref(),
        "ledger-archived-explicit",
        "local",
        "recall",
        None,
        None,
        None,
        &target,
        "class-archived",
        "raw query",
        1_000,
        Some("unattributed"),
    )
    .await
    .unwrap();

    let error = pack
        .dispatch(
            "brain.feedback",
            json!({
                "target_id": target,
                "signal": "implicit_positive",
                "served_by_profile_id": LIFECYCLE_PROFILE_ID,
                "scorer_run_id": "scorer-archived-explicit",
                "serve_ledger_id": "ledger-archived-explicit",
            }),
            &registry,
            &token,
        )
        .await
        .expect_err("null accounting must not accept an archived explicit profile");
    assert!(matches!(error, RuntimeError::InvalidInput(message) if message.contains("archived")));
    let row = crate::serve_ledger::get_serve_row(rt.sql().as_ref(), "ledger-archived-explicit")
        .await
        .unwrap()
        .unwrap();
    assert!(row.grade.is_none());
    assert!(row.scorer_run_id.is_none());
}

/// ADR-081 amendment regression, paired with the two forced-zero tests
/// above: a serve row stamped `unspecified` (no profile was ever selected
/// at serve time — never a failed read) must keep the legacy
/// binding/default resolution permitted, not collapse into the same
/// forced-zero state as `unattributed`.
#[tokio::test]
async fn serve_ledger_stored_unspecified_marker_permits_legacy_fallback() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let target = create_test_entity(&rt, &token).await;

    crate::serve_ledger::record_serve(
        rt.sql().as_ref(),
        "ledger-row-unspecified",
        "local",
        "recall",
        None,
        None,
        None,
        &target,
        "class-unspecified",
        "raw query",
        1_000,
        Some("unspecified"),
    )
    .await
    .expect("record_serve");

    let state_before = pack.snapshot().balanced_recall;

    let result = pack
        .dispatch(
            "brain.feedback",
            json!({
                "target_id": target,
                "signal": "implicit_positive",
                "scorer_run_id": "scorer-run-unspecified",
                "serve_ledger_id": "ledger-row-unspecified",
            }),
            &registry,
            &token,
        )
        .await
        .expect("stored-unspecified row must resolve via legacy binding/default fallback");

    let state_after = pack.snapshot().balanced_recall;
    assert_ne!(
        state_after, state_before,
        "a stored unspecified marker must keep the legacy binding/default \
             fallback permitted — it must NOT be forced to zero weight like a \
             genuine unattributed (failed profile read) row"
    );
    assert_eq!(result["serve_attribution"], json!("unspecified"));
    assert_eq!(
        result["served_by_profile_id"],
        json!("balanced-recall-v1"),
        "no explicit profile was supplied, so the default fallback profile must be credited"
    );
}

#[tokio::test]
async fn serve_ledger_missing_row_returns_not_found() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let target = create_test_entity(&rt, &token).await;

    let err = pack
        .dispatch(
            "brain.feedback",
            json!({
                "target_id": target,
                "signal": "implicit_positive",
                "scorer_run_id": "scorer-run-4",
                "serve_ledger_id": "does-not-exist",
            }),
            &registry,
            &token,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, RuntimeError::NotFound(_)));
}

#[test]
fn compute_query_class_is_deterministic_and_order_insensitive() {
    let a = crate::serve_ledger::compute_query_class("Rust async runtime tuning");
    let b = crate::serve_ledger::compute_query_class("tuning runtime async rust");
    let c = crate::serve_ledger::compute_query_class("Rust,  async! runtime  tuning.");
    assert_eq!(a, b, "token order must not affect the class key");
    assert_eq!(a, c, "punctuation/whitespace must not affect the class key");
    assert_eq!(a.len(), 16);

    let d = crate::serve_ledger::compute_query_class("completely different query");
    assert_ne!(a, d);
}

#[tokio::test]
async fn record_serve_duplicate_key_is_tolerated_not_errored() {
    let (_pack, rt) = make_pack();
    let token = rt.authorize(Namespace::local()).unwrap();
    let target = create_test_entity(&rt, &token).await;

    let first = crate::serve_ledger::record_serve(
        rt.sql().as_ref(),
        "row-a",
        "local",
        "recall",
        None,
        None,
        None,
        &target,
        "dup-class",
        "raw query",
        42_000,
        None,
    )
    .await
    .expect("first insert must succeed");
    assert!(first, "first write must report written=true");

    let second = crate::serve_ledger::record_serve(
        rt.sql().as_ref(),
        "row-b",
        "local",
        "recall",
        None,
        None,
        None,
        &target,
        "dup-class",
        "raw query",
        42_000,
        None,
    )
    .await
    .expect("duplicate key must be tolerated, not errored");
    assert!(!second, "exact-key duplicate must report written=false");
}

/// PR #583: a `UNIQUE` violation on the `id TEXT PRIMARY
/// KEY` column (same `id`, different natural key) must NOT be reported as
/// a tolerated natural-key duplicate — the row that actually exists does
/// not match `(namespace, target_id, query_class, served_at)`, so the
/// original error must propagate instead of being silently swallowed.
#[tokio::test]
async fn record_serve_primary_key_collision_with_different_natural_key_propagates_error() {
    let (_pack, rt) = make_pack();
    let token = rt.authorize(Namespace::local()).unwrap();
    let target_a = create_test_entity(&rt, &token).await;
    let target_b = create_test_entity(&rt, &token).await;

    crate::serve_ledger::record_serve(
        rt.sql().as_ref(),
        "shared-row-id",
        "local",
        "recall",
        None,
        None,
        None,
        &target_a,
        "class-a",
        "raw query a",
        1_000,
        None,
    )
    .await
    .expect("first insert must succeed");

    // Reuse the SAME id but a DIFFERENT natural key (different target_id) —
    // this collides on the primary key, not the natural-key unique index.
    let err = crate::serve_ledger::record_serve(
        rt.sql().as_ref(),
        "shared-row-id",
        "local",
        "recall",
        None,
        None,
        None,
        &target_b,
        "class-b",
        "raw query b",
        2_000,
        None,
    )
    .await
    .expect_err("a PK collision with a mismatched natural key must not be tolerated");
    assert!(
        matches!(err, RuntimeError::Storage(_)),
        "expected the original insert error to propagate, got {err:?}"
    );
}

#[tokio::test]
async fn handle_record_serve_writes_one_row_per_target_and_stamps_profile() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let target_a = create_test_entity(&rt, &token).await;
    let target_b = create_test_entity(&rt, &token).await;

    let result = pack
        .dispatch(
            "brain.record_serve",
            json!({
                "consumer_kind": "recall",
                "served_by_profile_id": "adr081-recall-v1",
                "target_ids": [target_a.clone(), target_b.clone()],
                "query_raw": "async runtime tuning",
            }),
            &registry,
            &token,
        )
        .await
        .expect("record_serve dispatch");
    assert_eq!(result["written"], json!(2));
    assert_eq!(result["skipped"], json!(0));

    let mut reader = rt.sql().reader().await.expect("reader");
    let raw_row = reader
        .query_row(khive_storage::types::SqlStatement {
            sql: "SELECT id FROM brain_serve_ledger WHERE target_id = ?1".into(),
            params: vec![khive_storage::types::SqlValue::Text(target_a.clone())],
            label: None,
        })
        .await
        .expect("query row")
        .expect("row for target_a must exist");
    let row_id = match raw_row.get("id") {
        Some(khive_storage::types::SqlValue::Text(s)) => s.clone(),
        other => panic!("expected text id, got {other:?}"),
    };
    drop(reader);

    let row = crate::serve_ledger::get_serve_row(rt.sql().as_ref(), &row_id)
        .await
        .unwrap()
        .expect("row must exist");
    assert_eq!(
        row.served_by_profile_id.as_deref(),
        Some("adr081-recall-v1")
    );
    assert_eq!(row.query_class, result["query_class"].as_str().unwrap());
}

#[tokio::test]
async fn handle_record_serve_tolerates_duplicate_target_in_same_batch_drain() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let target = create_test_entity(&rt, &token).await;

    let params = json!({
        "consumer_kind": "recall",
        "target_ids": [target.clone()],
        "query_raw": "same query twice",
        "served_at": 99_000,
    });

    let first = pack
        .dispatch("brain.record_serve", params.clone(), &registry, &token)
        .await
        .expect("first drain");
    assert_eq!(first["written"], json!(1));
    assert_eq!(first["skipped"], json!(0));

    let second = pack
        .dispatch("brain.record_serve", params, &registry, &token)
        .await
        .expect("second drain must not error");
    assert_eq!(second["written"], json!(0));
    assert_eq!(second["skipped"], json!(1));
}

#[tokio::test]
async fn handle_record_serve_unresolved_profile_still_writes_row() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let target = create_test_entity(&rt, &token).await;

    let result = pack
        .dispatch(
            "brain.record_serve",
            json!({
                "consumer_kind": "recall",
                "target_ids": [target.clone()],
                "query_raw": "unresolved profile query",
            }),
            &registry,
            &token,
        )
        .await
        .expect("record_serve dispatch");
    assert_eq!(result["written"], json!(1));

    let mut reader = rt.sql().reader().await.expect("reader");
    let raw_row = reader
        .query_row(khive_storage::types::SqlStatement {
            sql: "SELECT id, served_by_profile_id FROM brain_serve_ledger WHERE target_id = ?1"
                .into(),
            params: vec![khive_storage::types::SqlValue::Text(target)],
            label: None,
        })
        .await
        .expect("query row")
        .expect("row must exist");
    assert!(
        matches!(
            raw_row.get("served_by_profile_id"),
            Some(khive_storage::types::SqlValue::Null) | None
        ),
        "unresolved profile must write a null column, not a placeholder string"
    );
}

/// Replay determinism: a gated (zero-weight) event, once replayed from the
/// event log against a fresh BrainPack, must reproduce the exact same
/// (zero) posterior effect — not re-evaluate the gate against present-day
/// mass-table state (ADR-081 §2 replay-determinism design, see
/// `BrainSignal::SemanticFeedback::effective_weight` doc comment).
#[tokio::test]
async fn replay_reproduces_gated_zero_weight_deterministically() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let target = create_test_entity(&rt, &token).await;

    // Saturate the cap: 15 events at 0.1 = 1.5.
    for _ in 0..15 {
        pack.dispatch(
            "brain.feedback",
            json!({"target_id": target, "signal": "implicit_negative"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    }
    // 16th event must clamp to zero weight live.
    pack.dispatch(
        "brain.feedback",
        json!({"target_id": target, "signal": "implicit_negative"}),
        &registry,
        &token,
    )
    .await
    .unwrap();

    let live_sal_beta = pack.snapshot().balanced_recall.salience.beta();

    // Fresh pack over the same namespace: ensure_loaded will replay all 16
    // events from brain_event_log. If replay re-evaluated the gate instead
    // of reading the persisted effective_weight, it would see an empty
    // brain_implicit_mass table (or diverge from the live mass) and could
    // fold differently than the live run did.
    let pack2 = crate::BrainPack::new(rt.clone());
    pack2
        .dispatch("brain.profiles", json!({}), &registry, &token)
        .await
        .expect("dispatch triggers ensure_loaded");
    let replayed_sal_beta = pack2.snapshot().balanced_recall.salience.beta();

    assert!(
        (replayed_sal_beta - live_sal_beta).abs() < 1e-9,
        "replay must reproduce the live gated outcome exactly; \
             live={live_sal_beta}, replayed={replayed_sal_beta}"
    );
}
