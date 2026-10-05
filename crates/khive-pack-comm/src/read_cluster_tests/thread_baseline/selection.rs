use super::*;

#[tokio::test]
async fn thread_baseline_t3_visibility_precedes_fold_and_physical_cursor() {
    let (runtime, _, registry) = fixture();
    let root = id(900);
    let mut hidden = outbound(100, 400, root);
    hidden.properties.as_mut().unwrap()["from_actor"] = json!("lambda:hidden-sender");
    hidden.properties.as_mut().unwrap()["to_actor"] = json!("lambda:hidden-reader");
    let mut hidden_twin = inbound(103, 100, root, 102, json!(true));
    hidden_twin.properties.as_mut().unwrap()["to_actor"] = json!({});
    let mut foreign = inbound(104, 50, root, 102, json!(true));
    foreign.namespace = "lambda:foreign".into();
    seed(
        &runtime,
        vec![
            hidden,
            inbound(101, 300, root, 100, json!(true)),
            row(102, 200, root),
            hidden_twin,
            foreign,
        ],
    )
    .await;
    let (response, observation) = observed(
        &runtime,
        &registry,
        json!({"id":id(101).to_string(),"order":"desc","after":id(100).to_string()}),
    )
    .await;
    expect_ids(&response, &observation, &[id(101), id(102)]);
    assert_eq!(observation.fetched_rows, 4);
    assert_eq!(observation.retained_rows["visible"], 2);
    assert_eq!(response["messages"][0]["content"], "message 101");
    assert_eq!(response["messages"][0]["read"], true);
    assert_eq!(response["messages"][1]["read"], false);
    let (response, observation) = observed(
        &runtime,
        &registry,
        json!({"id":id(101).to_string(),"order":"desc","after":id(101).to_string(),"limit":1}),
    )
    .await;
    expect_ids(&response, &observation, &[id(102)]);
    assert_eq!(response["messages"][0]["read"], false);
}

#[tokio::test]
async fn thread_baseline_t3_both_addressed_participants_see_one_physical_owner() {
    let (runtime, _, registry) = fixture();
    let root = id(1);
    let mut twin = inbound(2, 200, root, 1, json!(true));
    twin.properties.as_mut().unwrap()["from_actor"] = json!("lambda:reader");
    twin.properties.as_mut().unwrap()["to_actor"] = json!("lambda:sender");
    seed(&runtime, vec![outbound(1, 100, root), twin]).await;
    for actor in ["lambda:reader", "lambda:sender"] {
        let (response, observation) = observed_as(
            &runtime,
            &registry,
            json!({"id":root.to_string()}),
            Some(actor),
        )
        .await;
        expect_ids(&response, &observation, &[root]);
        assert_eq!(observation.retained_rows["visible"], 2);
        assert_eq!(observation.retained_rows["folded"], 1);
        assert_eq!(response["messages"][0]["read"], true);
    }
}

#[tokio::test]
async fn thread_baseline_t4_cursor_uses_physical_twin_and_both_tuple_components() {
    let (runtime, _, registry) = fixture();
    let root = id(900);
    seed(
        &runtime,
        vec![
            outbound(10, 1_000_000, root),
            inbound(11, 2_000_000, root, 10, json!(true)),
            row(12, 1_500_000, root),
            row(13, 1_500_000, root),
            row(14, 2_500_000, root),
        ],
    )
    .await;
    for (order, after, expected) in [
        ("asc", None, vec![10, 12, 13, 14]),
        ("desc", None, vec![14, 13, 12, 10]),
        ("asc", Some(id(11).to_string()), vec![14]),
        ("desc", Some(id(11).to_string()), vec![13, 12, 10]),
        ("asc", Some(id(10).to_string()), vec![12, 13, 14]),
        ("desc", Some(id(10).to_string()), vec![]),
        ("asc", Some(id(12).to_string()), vec![13, 14]),
        ("desc", Some(id(12).to_string()), vec![10]),
        ("asc", Some(id(14).to_string()), vec![]),
        ("desc", Some(id(14).to_string()), vec![13, 12, 10]),
        (
            "asc",
            Some("1970-01-01T01:00:01.500000+01:00".into()),
            vec![14],
        ),
        (
            "desc",
            Some("1970-01-01T01:00:01.500000+01:00".into()),
            vec![10],
        ),
    ] {
        let mut params = json!({"id":id(10).to_string(),"order":order});
        if let Some(after) = after {
            params["after"] = json!(after);
        }
        let (response, observation) = observed(&runtime, &registry, params).await;
        let expected: Vec<_> = expected.into_iter().map(id).collect();
        expect_ids(&response, &observation, &expected);
        assert_eq!(observation.retained_rows["folded"], 4);
        assert_eq!(observation.retained_rows["after_cursor"], expected.len());
    }
    let (response, observation) = observed(
        &runtime,
        &registry,
        json!({"id":id(10).to_string(),"limit":0}),
    )
    .await;
    expect_ids(&response, &observation, &[id(10)]);
    for invalid in [
        json!({"order":"sideways"}),
        json!({"fields":["not-a-message-field"]}),
        json!({"after":"not-a-cursor"}),
    ] {
        let mut params = invalid;
        params["id"] = json!(id(10).to_string());
        assert!(matches!(
            registry.dispatch("comm.thread", params).await,
            Err(RuntimeError::InvalidInput(_))
        ));
    }
}

#[tokio::test]
async fn thread_baseline_t5_eight_spellings_plus_selected_exact_not_every_parseable_alias() {
    let (runtime, _, registry) = fixture();
    let root = Uuid::parse_str("abcdefab-cdef-abcd-efab-cdefabcdefab").unwrap();
    let spellings = [
        root.to_string(),
        root.simple().to_string(),
        root.braced().to_string(),
        root.urn().to_string(),
        format!("{:X}", root.as_hyphenated()),
        format!("{:X}", root.simple()),
        format!("{:X}", root.braced()),
        format!("{:X}", root.urn()),
    ];
    assert_eq!(spellings.iter().collect::<HashSet<_>>().len(), 8);
    let selected = "aBcDefAB-cDef-abCD-efAB-cdEFAbcDefAB";
    let excluded = "ABcDefAB-cDef-abCD-efAB-cdEFAbcDefAB";
    assert_eq!(selected.parse::<Uuid>().unwrap(), root);
    assert_eq!(excluded.parse::<Uuid>().unwrap(), root);
    assert!(!spellings
        .iter()
        .any(|spelling| spelling == selected || spelling == excluded));
    let mut canonical = row(99, 0, root);
    canonical.id = root;
    let mut notes = vec![canonical];
    for (n, spelling) in spellings
        .iter()
        .map(String::as_str)
        .chain([selected, excluded])
        .enumerate()
    {
        let mut note = row(n as u32 + 1, (n as i64 + 1) * 1_000_000, root);
        note.properties.as_mut().unwrap()["thread_id"] = json!(spelling);
        notes.push(note);
    }
    let mut padded = row(11, 11_000_000, root);
    padded.properties.as_mut().unwrap()["thread_id"] = json!(format!(" {selected} "));
    notes.push(padded);
    seed(&runtime, notes).await;
    for (passed, last, fetched, rendered) in
        [(root, 8, 9, 9), (id(9), 9, 10, 10), (id(11), 9, 10, 11)]
    {
        let (response, observation) =
            observed(&runtime, &registry, json!({"id":passed.to_string()})).await;
        let mut expected = vec![root];
        expected.extend((1..=last).map(id));
        if passed == id(11) {
            expected.push(id(11));
        }
        expect_ids(&response, &observation, &expected);
        assert_eq!(observation.fetched_rows, fetched);
        assert_eq!(observation.rendered_rows, rendered);
        assert_eq!(response["thread_id"], root.to_string());
        assert!(!thread_messages(&response).contains(&id(10).to_string()));
    }
}

#[tokio::test]
async fn thread_baseline_t5_missing_and_invalid_root_metadata_fall_back_to_selected_id() {
    for raw in [
        None,
        Some(json!("not-a-uuid")),
        Some(json!("   ")),
        Some(json!({})),
    ] {
        let (runtime, _, registry) = fixture();
        let mut selected = message(1);
        if let Some(raw) = raw {
            selected.properties.as_mut().unwrap()["thread_id"] = raw;
        }
        seed(&runtime, vec![selected, row(2, 2_000_000, id(1))]).await;
        let (response, observation) =
            observed(&runtime, &registry, json!({"id":id(1).to_string()})).await;
        expect_ids(&response, &observation, &[id(1), id(2)]);
        assert_eq!(response["thread_id"], id(1).to_string());
        assert_eq!(observation.fetched_rows, 1);
        assert_eq!(observation.rendered_rows, 2);
    }
}
