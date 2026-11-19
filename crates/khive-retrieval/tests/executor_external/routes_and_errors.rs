#[tokio::test]
async fn unknown_custom_returns_the_original_typed_error_before_zero_limit_work() {
    let trace = Mutex::new(Trace::default());
    let ns = ["ns"];
    let mode = TextQueryMode::Plain;
    let arm = FakeArm::new(&trace, completion(vec![], ArmRoute::Primary, "empty"));
    let arms = [ArmSpec {
        key: "text",
        adapter: &arm,
        scope: SearchScope {
            namespaces: &ns,
            filter: "all",
        },
        input: Some(ArmInput::Text {
            query: "q",
            mode: &mode,
            snippet_chars: 0,
        }),
        candidate_limit: 0,
        failure: FailurePolicy::Required,
    }];
    let strategy = FusionStrategy::Custom {
        name: "unregistered".into(),
        params: serde_json::json!({"opaque": 5}),
    };
    let error = fault(41);
    let original_witness = (&*error.witness) as *const Cell<u8> as usize;
    let result = execute_hybrid(
        request(&arms, &ns, "all", 0, 0),
        &Hydrator {
            trace: &trace,
            flow: Flow::Pass,
        },
        || {
            trace.lock().unwrap().phases.push("resolve");
            assert!(matches!(
                &strategy,
                FusionStrategy::Custom { name, params }
                    if name == "unregistered" && params["opaque"] == 5
            ));
            Err::<fn(Vec<Vec<Row>>) -> Ready<Result<Vec<Row>, OpaqueError>>, _>(error)
        },
    )
    .await;
    match result {
        Err(ExecutorError::FusionResolution(error)) => {
            assert_eq!(error.code, 41);
            assert_eq!(
                (&*error.witness) as *const Cell<u8> as usize,
                original_witness
            );
        }
        _ => panic!("expected the unresolved Custom error"),
    }
    let trace = trace.lock().unwrap();
    assert!(trace.requests.is_empty());
    assert_eq!(trace.phases, ["resolve"]);
}

fn text_spec<'a>(
    key: &'a str,
    adapter: &'a FakeArm<'a>,
    namespaces: &'a [&'a str],
    mode: &'a TextQueryMode,
    limit: u32,
    failure: FailurePolicy,
) -> ArmSpec<'a, FakeArm<'a>> {
    ArmSpec {
        key,
        adapter,
        scope: SearchScope {
            namespaces,
            filter: "all",
        },
        input: Some(ArmInput::Text {
            query: "q",
            mode,
            snippet_chars: 0,
        }),
        candidate_limit: limit,
        failure,
    }
}

#[tokio::test]
async fn empty_unavailable_failed_and_exact_routes_remain_distinct_evidence() {
    let trace = Mutex::new(Trace::default());
    let ns = ["ns"];
    let mode = TextQueryMode::Plain;
    let empty = FakeArm::new(
        &trace,
        completion(vec![], ArmRoute::Primary, "healthy-empty"),
    );
    let unavailable = FakeArm::new(
        &trace,
        Err(ArmFailure::Unavailable(Evidence("not-built".into()))),
    );
    let failed = FakeArm::new(&trace, Err(ArmFailure::Ordinary(fault(12))));
    let exact = FakeArm::new(
        &trace,
        completion(
            vec![row(8, 0.8, "exact")],
            ArmRoute::ExactFallback,
            "index-unavailable",
        ),
    );
    let arms = [
        text_spec("empty", &empty, &ns, &mode, 1, FailurePolicy::Required),
        text_spec(
            "unavailable",
            &unavailable,
            &ns,
            &mode,
            1,
            FailurePolicy::TolerateOrdinary,
        ),
        text_spec(
            "failed",
            &failed,
            &ns,
            &mode,
            1,
            FailurePolicy::TolerateOrdinary,
        ),
        text_spec("exact", &exact, &ns, &mode, 1, FailurePolicy::Required),
    ];
    let out = success(
        execute_hybrid(
            request(&arms, &ns, "all", 4, 4),
            &Hydrator {
                trace: &trace,
                flow: Flow::Pass,
            },
            || {
                Ok(|streams: Vec<Vec<Row>>| {
                    assert_eq!(
                        streams.iter().map(Vec::len).collect::<Vec<_>>(),
                        [0, 0, 0, 1]
                    );
                    concat(streams)
                })
            },
        )
        .await,
    );
    assert_eq!(out.hits.iter().map(|hit| hit.id).collect::<Vec<_>>(), [8]);
    assert_eq!(
        out.arms
            .iter()
            .map(|report| (report.key.as_str(), report.requested, report.returned))
            .collect::<Vec<_>>(),
        [
            ("empty", 1, 0),
            ("unavailable", 1, 0),
            ("failed", 1, 0),
            ("exact", 1, 1)
        ]
    );
    let empty_report: &ArmReport<Evidence, OpaqueError> = &out.arms[0];
    assert!(matches!(
        &empty_report.state,
        ArmState::Completed { route: ArmRoute::Primary, evidence }
            if evidence.0 == "healthy-empty"
    ));
    assert!(
        matches!(&out.arms[1].state, ArmState::Unavailable(evidence) if evidence.0 == "not-built")
    );
    assert!(matches!(
        &out.arms[2].state,
        ArmState::Failed(error) if error.code == 12 && error.witness.get() == 77
    ));
    assert!(matches!(
        &out.arms[3].state,
        ArmState::Completed { route: ArmRoute::ExactFallback, evidence }
            if evidence.0 == "index-unavailable"
    ));
    assert_eq!(trace.lock().unwrap().requests.len(), 4);
}

#[tokio::test]
async fn required_unavailability_and_tolerated_fatal_errors_do_not_become_empty_success() {
    for fatal in [false, true] {
        let trace = Mutex::new(Trace::default());
        let ns = ["ns"];
        let mode = TextQueryMode::Plain;
        let failure = if fatal {
            ArmFailure::Fatal(fault(19))
        } else {
            ArmFailure::Unavailable(Evidence("required-index".into()))
        };
        let arm = FakeArm::new(&trace, Err(failure));
        let policy = if fatal {
            FailurePolicy::TolerateOrdinary
        } else {
            FailurePolicy::Required
        };
        let arms = [text_spec("required", &arm, &ns, &mode, 1, policy)];
        let result = execute_hybrid(
            request(&arms, &ns, "all", 1, 1),
            &Hydrator {
                trace: &trace,
                flow: Flow::Pass,
            },
            || Ok(concat),
        )
        .await;
        match result {
            Err(ExecutorError::Unavailable { slot: 0, evidence }) if !fatal => {
                assert_eq!(evidence.0, "required-index")
            }
            Err(ExecutorError::Arm { slot: 0, error }) if fatal => assert_eq!(error.code, 19),
            _ => panic!("required absence or fatal error was swallowed"),
        }
        assert!(trace.lock().unwrap().phases.is_empty());
    }
}

#[tokio::test]
async fn selected_zero_window_still_runs_and_returns_its_typed_failure() {
    let trace = Mutex::new(Trace::default());
    let ns = ["ns"];
    let mode = TextQueryMode::Plain;
    let arm = FakeArm::new(&trace, Err(ArmFailure::Ordinary(fault(23))));
    let arms = [text_spec(
        "zero",
        &arm,
        &ns,
        &mode,
        0,
        FailurePolicy::Required,
    )];
    let result = execute_hybrid(
        request(&arms, &ns, "all", 0, 0),
        &Hydrator {
            trace: &trace,
            flow: Flow::Pass,
        },
        || Ok(concat),
    )
    .await;
    assert!(matches!(result, Err(ExecutorError::Arm { slot: 0, error }) if error.code == 23));
    let trace = trace.lock().unwrap();
    assert_eq!(trace.requests.len(), 1);
    assert_eq!(trace.requests[0].limit, 0);
    assert!(trace.phases.is_empty());
}
