#[tokio::test]
async fn external_preparer_preserves_original_slots_and_owned_embedding_lifetime() {
    let trace = Mutex::new(Trace::default());
    let unused = FakeArm::new(
        &trace,
        completion(vec![], ArmRoute::Primary, "must-not-run"),
    );
    let selected = FakeArm::new(
        &trace,
        completion(
            vec![row(7, 0.7, "b")],
            ArmRoute::ExactFallback,
            "index-absent",
        ),
    );
    let ns = ["ns"];
    let query = String::from("borrowed query");
    let arms = [
        ArmSpec {
            key: "skipped",
            adapter: &unused,
            scope: SearchScope {
                namespaces: &ns,
                filter: "kind=memory",
            },
            input: None,
            candidate_limit: 7,
            failure: FailurePolicy::Required,
        },
        ArmSpec {
            key: "selected",
            adapter: &selected,
            scope: SearchScope {
                namespaces: &ns,
                filter: "kind=memory",
            },
            input: Some(ArmInput::Vector {
                engine: "second-engine",
                model_id: "model-b",
                space_id: Some("space-b"),
                input: VectorInput::Text(&query),
            }),
            candidate_limit: 1,
            failure: FailurePolicy::Required,
        },
    ];
    let preparer = Preparer(&trace);
    let external_preparer: &dyn VectorGroupPreparer<
        Filter = str,
        Evidence = Evidence,
        Error = OpaqueError,
    > = &preparer;
    let mut req = request(&arms, &ns, "kind=memory", 1, 1);
    req.vector_slots = &[0, 1];
    req.vector_preparer = Some(external_preparer);
    let out = success(
        execute_memory_hybrid(
            req,
            &Hydrator {
                trace: &trace,
                flow: Flow::Pass,
            },
            || Ok(concat),
        )
        .await,
    );
    assert_eq!(out.hits.iter().map(|h| h.id).collect::<Vec<_>>(), vec![7]);
    assert_eq!(
        out.vector_preparation.expect("aggregate evidence").0,
        "session-proof-retained"
    );
    assert!(matches!(out.arms[0].state, ArmState::Skipped));
    assert_eq!(
        out.arms[1]
            .preparation
            .as_ref()
            .expect("per-model evidence")
            .0,
        "model-prepared"
    );
    assert!(matches!(
        &out.arms[1].state,
        ArmState::Completed { route: ArmRoute::ExactFallback, evidence }
            if evidence.0 == "index-absent"
    ));
    let trace = trace.lock().unwrap();
    assert_eq!(trace.requests.len(), 1);
    assert_eq!(
        trace.requests[0].input,
        SeenInput::Vector(
            "second-engine".into(),
            "model-b".into(),
            Some("space-b".into()),
            None,
            vec![0.5, 0.25]
        )
    );
}
