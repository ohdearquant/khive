#[tokio::test]
async fn external_caller_preserves_scopes_and_batches_eligibility_before_ranking_and_cut() {
    let trace = Mutex::new(Trace::default());
    let namespace = String::from("visible-a");
    let other = String::from("visible-b");
    let all = [namespace.as_str(), other.as_str()];
    let primary = [namespace.as_str()];
    let filter = String::from("kind=note;tag=visible");
    let query = String::from("query text");
    let mode = TextQueryMode::AnyTerm;
    let vector = [0.25, 0.75];
    let vector_arm = FakeArm::new(
        &trace,
        completion(
            vec![
                row(1, 0.99, "vector-1"),
                row(2, 0.9, "vector-2"),
                row(3, 0.2, "vector-3"),
            ],
            ArmRoute::Primary,
            "primary-complete",
        ),
    );
    let text_arm = FakeArm::new(
        &trace,
        completion(
            vec![row(4, 0.1, "text-4")],
            ArmRoute::Primary,
            "text-complete",
        ),
    );
    let vector_adapter: &dyn ExecutorArm<
        Id = u8,
        Label = Label,
        Evidence = Evidence,
        Filter = str,
        Error = OpaqueError,
    > = &vector_arm;
    let text_adapter: &dyn ExecutorArm<
        Id = u8,
        Label = Label,
        Evidence = Evidence,
        Filter = str,
        Error = OpaqueError,
    > = &text_arm;
    let arms = [
        ArmSpec {
            key: "vector",
            adapter: vector_adapter,
            scope: SearchScope {
                namespaces: &primary,
                filter: &filter,
            },
            input: Some(ArmInput::Vector {
                engine: "named-engine",
                model_id: "canonical-model",
                space_id: Some("verified-space"),
                input: VectorInput::Embedding(&vector),
            }),
            candidate_limit: 3,
            failure: FailurePolicy::Required,
        },
        ArmSpec {
            key: "text",
            adapter: text_adapter,
            scope: SearchScope {
                namespaces: &all,
                filter: &filter,
            },
            input: Some(ArmInput::Text {
                query: &query,
                mode: &mode,
                snippet_chars: 37,
            }),
            candidate_limit: 1,
            failure: FailurePolicy::Required,
        },
    ];
    let hydrator = Hydrator {
        trace: &trace,
        flow: Flow::EligibilityAndSalience,
    };
    let external_hydrator: &dyn CandidateHydrator<
        Id = u8,
        Label = Label,
        Evidence = Evidence,
        Filter = str,
        Item = Item,
        State = BatchState,
        Error = OpaqueError,
    > = &hydrator;
    let mut req = request(&arms, &all, &filter, 4, 1);
    req.vector_slots = &[0];
    // The returned future borrows an outer environment; it is not 'static.
    let fusion_marker = String::from("caller-owned-fusion");
    let marker = fusion_marker.as_str();
    let out: Outcome = success(
        execute_hybrid(req, external_hydrator, || {
            trace.lock().unwrap().phases.push("resolve");
            Ok(move |streams: Vec<Vec<Row>>| async move {
                assert_eq!(marker, "caller-owned-fusion");
                let ids: Vec<Vec<_>> = streams
                    .iter()
                    .map(|arm| arm.iter().map(|r| r.0).collect())
                    .collect();
                assert_eq!(
                    ids,
                    vec![vec![2, 3], vec![4]],
                    "eligibility precedes fusion"
                );
                Ok(streams.into_iter().flatten().collect())
            })
        })
        .await,
    );
    assert_eq!(out.hits.iter().map(|h| h.id).collect::<Vec<_>>(), vec![4]);
    assert_eq!(out.hits[0].score, DeterministicScore::from_f64(0.4));
    assert_eq!(out.hits[0].label.0, "text-4");
    assert_eq!(out.hits[0].item.0, "loaded-4");
    assert_eq!(
        out.arms
            .iter()
            .map(|arm| (arm.requested, arm.returned))
            .collect::<Vec<_>>(),
        [(3, 3), (1, 1)]
    );
    let trace = trace.lock().unwrap();
    assert_eq!(trace.phases, ["resolve", "outcomes", "prepare", "hydrate"]);
    assert_eq!(
        trace.prepared_ids,
        vec![vec![1, 2, 3, 4]],
        "one batch, full raw union"
    );
    assert_eq!(
        trace.hydration_scopes,
        vec![(vec![namespace.clone(), other.clone()], filter.clone()); 2]
    );
    let seen_vector = trace
        .requests
        .iter()
        .find(|r| matches!(r.input, SeenInput::Vector(..)))
        .unwrap();
    assert_eq!(seen_vector.namespaces, ["visible-a"]);
    assert_eq!(seen_vector.filter, filter);
    assert_eq!(seen_vector.limit, 3);
    assert_eq!(
        seen_vector.input,
        SeenInput::Vector(
            "named-engine".into(),
            "canonical-model".into(),
            Some("verified-space".into()),
            None,
            vector.to_vec()
        )
    );
    let seen_text = trace
        .requests
        .iter()
        .find(|r| matches!(r.input, SeenInput::Text(..)))
        .unwrap();
    assert_eq!(seen_text.namespaces, ["visible-a", "visible-b"]);
    assert_eq!(seen_text.filter, filter);
    assert_eq!(seen_text.limit, 1);
    assert_eq!(seen_text.input, SeenInput::Text(query, mode, 37));
}
