use super::*;

#[tokio::test]
async fn thread_baseline_t6_multi_inbound_first_before_last_after_promotion() {
    let root = id(900);
    let cases = [
        (vec![(10, 400, true), (20, 300, false)], true, 50, true),
        (vec![(60, 100, true), (70, 50, false)], true, 50, false),
        (
            vec![(10, 400, false), (20, 300, true), (60, 100, true)],
            true,
            50,
            true,
        ),
        (
            vec![(10, 400, true), (20, 300, false), (60, 100, false)],
            true,
            50,
            false,
        ),
        (vec![(10, 400, true), (20, 300, false)], false, 10, true),
        (vec![(10, 200, false), (60, 200, true)], true, 50, true),
    ];
    for (twins, has_outbound, owner, expected_read) in cases {
        let (runtime, _, registry) = fixture();
        let mut notes: Vec<_> = twins
            .iter()
            .map(|(index, time, read)| inbound(*index, *time, root, 50, json!(read)))
            .collect();
        let selected = if has_outbound {
            notes.push(outbound(50, 200, root));
            id(50)
        } else {
            id(twins[0].0)
        };
        seed(&runtime, notes).await;
        for order in ["asc", "desc"] {
            let (response, observation) = observed(
                &runtime,
                &registry,
                json!({"id":selected.to_string(),"order":order}),
            )
            .await;
            expect_ids(&response, &observation, &[id(owner)]);
            assert_eq!(response["messages"][0]["read"], expected_read);
            assert_eq!(response["messages"][0]["properties"]["read"], !has_outbound);
            assert_eq!(observation.retained_rows["folded"], 1);
        }
    }
}

#[tokio::test]
async fn thread_baseline_t6_non_boolean_read_renders_false_without_rewriting_properties() {
    for raw in [
        None,
        Some(Value::Null),
        Some(json!("true")),
        Some(json!(1)),
        Some(json!({})),
    ] {
        let (runtime, _, registry) = fixture();
        let root = id(900);
        let mut twin = inbound(60, 100, root, 50, Value::Null);
        let props = twin.properties.as_mut().unwrap().as_object_mut().unwrap();
        match &raw {
            Some(raw) => {
                props.insert("read".into(), raw.clone());
            }
            None => {
                props.remove("read");
            }
        }
        seed(&runtime, vec![outbound(50, 200, root), twin]).await;
        let (response, observation) =
            observed(&runtime, &registry, json!({"id":id(50).to_string()})).await;
        expect_ids(&response, &observation, &[id(50)]);
        assert_eq!(response["messages"][0]["read"], false);
        assert_eq!(response["messages"][0]["properties"]["read"], false);
        let stored = runtime
            .backend()
            .notes()
            .unwrap()
            .get_note(id(60))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.properties.unwrap().get("read"), raw.as_ref());
        let (runtime, _, registry) = fixture();
        let mut only = row(60, 100, root);
        match &raw {
            Some(raw) => {
                only.properties.as_mut().unwrap()["read"] = raw.clone();
            }
            None => {
                only.properties
                    .as_mut()
                    .unwrap()
                    .as_object_mut()
                    .unwrap()
                    .remove("read");
            }
        }
        seed(&runtime, vec![only]).await;
        let (response, observation) =
            observed(&runtime, &registry, json!({"id":id(60).to_string()})).await;
        expect_ids(&response, &observation, &[id(60)]);
        assert_eq!(response["messages"][0]["read"], false);
        assert_eq!(
            response["messages"][0]["properties"].get("read"),
            raw.as_ref()
        );
    }
}

#[tokio::test]
async fn thread_baseline_t6_reference_is_one_hop_and_malformed_reference_is_own_id() {
    let (runtime, _, registry) = fixture();
    let root = id(900);
    let mut malformed = row(30, 300, root);
    malformed.properties.as_mut().unwrap()["outbound_ref"] = json!("not-a-uuid");
    seed(
        &runtime,
        vec![
            outbound(50, 100, root),
            inbound(10, 400, root, 50, json!(true)),
            inbound(20, 200, root, 10, json!(false)),
            malformed,
        ],
    )
    .await;
    let (response, observation) =
        observed(&runtime, &registry, json!({"id":id(50).to_string()})).await;
    expect_ids(&response, &observation, &[id(50), id(20), id(30)]);
    assert_eq!(response["messages"][0]["read"], true);
    assert_eq!(observation.retained_rows["folded"], 3);
}

#[tokio::test]
async fn thread_baseline_t6_selected_physical_child_appends_after_the_scan() {
    let (runtime, _, registry) = fixture();
    let root = id(900);
    let mut selected = inbound(5, 400, root, 3, json!(false));
    selected.properties.as_mut().unwrap()["thread_id"] = json!(format!(" {} ", root));
    seed(
        &runtime,
        vec![
            inbound(2, 300, root, 3, json!(true)),
            outbound(3, 200, root),
            inbound(4, 100, root, 3, json!(true)),
            selected,
        ],
    )
    .await;
    let (response, observation) =
        observed(&runtime, &registry, json!({"id":id(5).to_string()})).await;
    expect_ids(&response, &observation, &[id(3)]);
    assert_eq!(response["thread_id"], root.to_string());
    assert_eq!(observation.fetched_rows, 3);
    assert_eq!(observation.rendered_rows, 4);
    assert_eq!(
        response["messages"][0]["read"], false,
        "selected child is appended last even though its timestamp is newest"
    );
    assert_eq!(response["messages"][0]["properties"]["read"], false);
    assert!(runtime
        .backend()
        .notes()
        .unwrap()
        .get_note(root)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn thread_baseline_t6_selected_outbound_append_promotes_the_physical_owner() {
    let (runtime, _, registry) = fixture();
    let root = id(900);
    let mut selected = outbound(3, 300, root);
    selected.properties.as_mut().unwrap()["thread_id"] = json!(format!(" {} ", root));
    seed(
        &runtime,
        vec![inbound(2, 100, root, 3, json!(true)), selected],
    )
    .await;
    let (response, observation) = observed(
        &runtime,
        &registry,
        json!({"id":id(3).to_string(),"fields":["read","properties"]}),
    )
    .await;
    assert_eq!(observation.final_owner_ids, vec![id(3).to_string()]);
    assert_eq!(observation.fetched_rows, 1);
    assert_eq!(observation.rendered_rows, 2);
    assert_eq!(response["messages"][0]["read"], true);
    assert_eq!(response["messages"][0]["properties"]["read"], false);
    assert!(response["messages"][0].get("full_id").is_none());
}
