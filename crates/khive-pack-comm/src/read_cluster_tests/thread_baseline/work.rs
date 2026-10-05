use super::*;

async fn matrix(count: u32) {
    let (runtime, _, registry) = fixture();
    let root = id(1);
    seed(
        &runtime,
        (1..=count)
            .map(|index| row(index, i64::from(index) * 1_000_000, root))
            .collect(),
    )
    .await;
    let cursors = [1, count / 2, count];
    for limit in [1, 20, 100, 500] {
        for order in ["asc", "desc"] {
            let mut after = vec![None];
            for index in cursors {
                after.push(Some((json!(id(index).to_string()), index)));
                after.push(Some((
                    json!(micros_to_iso(i64::from(index) * 1_000_000)),
                    index,
                )));
            }
            for cursor in after {
                let mut params = json!({"id":root.to_string(),"limit":limit,"order":order});
                let cursor_index = cursor.as_ref().map(|(_, index)| *index);
                if let Some((value, _)) = cursor {
                    params["after"] = value;
                }
                let (response, observation) = observed(&runtime, &registry, params.clone()).await;
                let mut indices: Vec<_> = (1..=count)
                    .filter(|index| {
                        cursor_index.is_none_or(|cursor| match order {
                            "asc" => *index > cursor,
                            _ => *index < cursor,
                        })
                    })
                    .collect();
                if order == "desc" {
                    indices.reverse();
                }
                let eligible = indices.len();
                indices.truncate(limit);
                let expected: Vec<_> = indices.into_iter().map(id).collect();
                expect_ids(&response, &observation, &expected);
                assert_eq!(observation.fetched_rows, count as usize);
                assert_eq!(observation.rendered_rows, count as usize);
                assert_eq!(observation.peak_physical_rows, count as usize);
                assert_eq!(observation.physical_pages, count as usize / 200 + 1);
                assert_eq!(observation.retained_rows["visible"], count as usize);
                assert_eq!(observation.retained_rows["folded"], count as usize);
                assert_eq!(observation.retained_rows["after_cursor"], eligible);
                assert_eq!(observation.retained_rows["limited"], expected.len());
                assert!(!observation.statement_starts.is_empty());
                emit(&format!("unique-{count}"), &params, &response, &observation);
            }
        }
    }
}

#[tokio::test]
async fn thread_baseline_t1_one_thousand_physical_rows() {
    matrix(1_000).await;
    discarded(1_000).await;
}

#[tokio::test]
#[ignore = "explicit 10k real-handler baseline measurement"]
async fn thread_baseline_t1_ten_thousand_physical_rows() {
    matrix(10_000).await;
    discarded(10_000).await;
}

#[tokio::test]
#[ignore = "explicit 100k real-handler baseline measurement"]
async fn thread_baseline_t1_one_hundred_thousand_physical_rows() {
    matrix(100_000).await;
    discarded(100_000).await;
}

#[tokio::test]
async fn thread_baseline_t1_discarded_physical_rows_still_render() {
    discarded(211).await;
}

async fn discarded(count: u32) {
    for population in ["invisible", "inbound-aliases", "demoted-inbound-heads"] {
        let (runtime, _, registry) = fixture();
        let root = id(1);
        let demoted = population == "demoted-inbound-heads";
        let mut notes = vec![outbound(1, if demoted { 0 } else { 1_000_000 }, root)];
        for index in 2..=count {
            let mut note = inbound(
                index,
                if demoted { i64::from(index) } else { 1 },
                root,
                1,
                json!(true),
            );
            if population == "invisible" {
                let props = note.properties.as_mut().unwrap();
                props["from_actor"] = json!("lambda:hidden-sender");
                props["to_actor"] = json!("lambda:hidden-reader");
            }
            notes.push(note);
        }
        seed(&runtime, notes).await;
        let params = json!({"id":root.to_string(),"limit":1,"order":"desc"});
        let (response, observation) = observed(&runtime, &registry, params.clone()).await;
        expect_ids(&response, &observation, &[root]);
        assert_eq!(observation.fetched_rows, count as usize);
        assert_eq!(observation.rendered_rows, count as usize);
        assert_eq!(observation.peak_physical_rows, count as usize);
        assert_eq!(
            observation.retained_rows["visible"],
            if population == "invisible" {
                1
            } else {
                count as usize
            }
        );
        assert_eq!(observation.retained_rows["folded"], 1);
        assert_eq!(response["messages"][0]["read"], population != "invisible");
        emit(
            &format!("{population}-{count}"),
            &params,
            &response,
            &observation,
        );
    }
}

#[tokio::test]
async fn thread_baseline_t2_late_twin_keeps_outbound_owner_and_property_read() {
    let (runtime, _, registry) = fixture();
    let root = id(1);
    let mut notes = vec![outbound(1, 2_000_000, root)];
    notes.extend((2..=1_001).map(|index| row(index, 1_000_000, root)));
    notes.push(inbound(1_002, 1, root, 1, json!(true)));
    seed(&runtime, notes).await;
    let (response, observation) = observed(
        &runtime,
        &registry,
        json!({"id":root.to_string(),"limit":1,"order":"desc"}),
    )
    .await;
    expect_ids(&response, &observation, &[root]);
    assert_eq!(response["messages"][0]["content"], "message 1");
    assert_eq!(
        response["messages"][0]["created_at"],
        micros_to_iso(2_000_000)
    );
    assert_eq!(response["messages"][0]["read"], true);
    assert_eq!(response["messages"][0]["properties"]["read"], false);
    assert_eq!(observation.physical_pages, 6);
    assert_eq!(observation.rendered_rows, 1_002);
    assert_eq!(observation.retained_rows["folded"], 1_001);
}
