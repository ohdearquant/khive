#[tokio::test]
async fn adapter_candidate_cap_is_enforced_before_hydration_or_fusion() {
    let trace = Mutex::new(Trace::default());
    let ns = ["ns"];
    let mode = TextQueryMode::Plain;
    let arm = FakeArm::new(
        &trace,
        completion(
            vec![row(1, 0.9, "a"), row(2, 0.8, "b")],
            ArmRoute::Primary,
            "too-many",
        ),
    );
    let arms = [text_spec(
        "over-cap",
        &arm,
        &ns,
        &mode,
        1,
        FailurePolicy::Required,
    )];
    let result = execute_hybrid(
        request(&arms, &ns, "all", 1, 1),
        &Hydrator {
            trace: &trace,
            flow: Flow::Pass,
        },
        || {
            Ok(|_: Vec<Vec<Row>>| {
                trace.lock().unwrap().phases.push("unexpected-fusion");
                concat(vec![])
            })
        },
    )
    .await;
    assert!(matches!(
        result,
        Err(ExecutorError::Protocol { slot: Some(0), .. })
    ));
    assert!(trace.lock().unwrap().phases.is_empty());
}

#[tokio::test]
async fn outcome_validation_cannot_rewrite_candidates_scores_or_raw_accounting() {
    for flow in [
        Flow::AddOutcome,
        Flow::ChangeOutcomeScore,
        Flow::ChangeAccounting,
    ] {
        let trace = Mutex::new(Trace::default());
        let ns = ["ns"];
        let mode = TextQueryMode::Plain;
        let arm = FakeArm::new(
            &trace,
            completion(vec![row(1, 0.9, "original")], ArmRoute::Primary, "one"),
        );
        let arms = [text_spec(
            "one",
            &arm,
            &ns,
            &mode,
            2,
            FailurePolicy::Required,
        )];
        let result = execute_hybrid(
            request(&arms, &ns, "all", 2, 2),
            &Hydrator {
                trace: &trace,
                flow,
            },
            || Ok(concat),
        )
        .await;
        assert!(matches!(result, Err(ExecutorError::Protocol { .. })));
        assert_eq!(trace.lock().unwrap().phases, ["outcomes"]);
    }
}

#[tokio::test]
async fn preparing_and_hydrating_must_not_add_identities_or_duplicate_multiplicity() {
    for flow in [
        Flow::AddBefore,
        Flow::DuplicateBefore,
        Flow::AddAfter,
        Flow::DuplicateAfter,
    ] {
        let trace = Mutex::new(Trace::default());
        let ns = ["ns"];
        let mode = TextQueryMode::Plain;
        let arm = FakeArm::new(
            &trace,
            completion(vec![row(1, 0.9, "original")], ArmRoute::Primary, "one"),
        );
        // Two slots of headroom isolate identity/multiplicity validation from cap validation.
        let arms = [text_spec(
            "one",
            &arm,
            &ns,
            &mode,
            2,
            FailurePolicy::Required,
        )];
        let result = execute_hybrid(
            request(&arms, &ns, "all", 2, 2),
            &Hydrator {
                trace: &trace,
                flow,
            },
            || Ok(concat),
        )
        .await;
        assert!(matches!(result, Err(ExecutorError::Protocol { .. })));
        assert_eq!(trace.lock().unwrap().prepared_ids, [vec![1]]);
    }
}

#[tokio::test]
async fn fusion_must_not_add_identities_or_duplicate_multiplicity() {
    for id in [99, 1] {
        let trace = Mutex::new(Trace::default());
        let ns = ["ns"];
        let mode = TextQueryMode::Plain;
        let arm = FakeArm::new(
            &trace,
            completion(vec![row(1, 0.9, "original")], ArmRoute::Primary, "one"),
        );
        let arms = [text_spec(
            "one",
            &arm,
            &ns,
            &mode,
            2,
            FailurePolicy::Required,
        )];
        let result = execute_hybrid(
            request(&arms, &ns, "all", 2, 2),
            &Hydrator {
                trace: &trace,
                flow: Flow::Pass,
            },
            || {
                Ok(move |streams: Vec<Vec<Row>>| {
                    let mut rows: Vec<_> = streams.into_iter().flatten().collect();
                    rows.push(row(id, 0.8, "injected"));
                    ready(Ok(rows))
                })
            },
        )
        .await;
        assert!(matches!(result, Err(ExecutorError::Protocol { .. })));
        assert_eq!(trace.lock().unwrap().phases, ["outcomes", "prepare"]);
    }
}

#[tokio::test]
async fn callback_errors_remain_owned_typed_values_without_display_or_debug() {
    for flow in [
        Flow::FailOutcomes,
        Flow::FailPreparation,
        Flow::FailHydration,
    ] {
        let trace = Mutex::new(Trace::default());
        let ns = ["ns"];
        let mode = TextQueryMode::Plain;
        let arm = FakeArm::new(
            &trace,
            completion(vec![row(1, 0.9, "original")], ArmRoute::Primary, "one"),
        );
        let arms = [text_spec(
            "one",
            &arm,
            &ns,
            &mode,
            1,
            FailurePolicy::Required,
        )];
        let result = execute_hybrid(
            request(&arms, &ns, "all", 1, 1),
            &Hydrator {
                trace: &trace,
                flow,
            },
            || Ok(concat),
        )
        .await;
        match (flow, result) {
            (Flow::FailOutcomes, Err(ExecutorError::OutcomePolicy(error))) => {
                assert_eq!(error.code, 31)
            }
            (Flow::FailPreparation, Err(ExecutorError::Preparation(error))) => {
                assert_eq!(error.code, 32)
            }
            (Flow::FailHydration, Err(ExecutorError::Hydration(error))) => {
                assert_eq!(error.code, 33)
            }
            _ => panic!("typed callback error was lost or moved to the wrong phase"),
        }
    }
}
