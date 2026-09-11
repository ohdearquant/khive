use super::*;

fn mixed_surface() -> (KhiveRuntime, VerbRegistry) {
    let (rt, registry) = surface();
    rt.install_kind_registry(
        registry
            .all_entity_kinds()
            .into_iter()
            .map(str::to_string)
            .collect(),
        registry
            .all_note_kinds()
            .into_iter()
            .map(str::to_string)
            .collect(),
    );
    (rt, registry)
}

#[tokio::test]
async fn ordered_fences_mixed_batch_uses_each_transaction_start_revision() {
    for atomic in [true, false] {
        for shape in 0..3 {
            let (_, registry) = mixed_surface();
            let a = lease(&registry, "lease/a").await;
            let b = lease(&registry, "lease/b").await;
            // Atomic fences precede both writes; per-member fences see the
            // revision committed by each preceding write transaction.
            let expected = if atomic { 1 } else { 2 };
            let (first, last) = match shape {
                0 => (fence("lease/a", expected), fence("lease/b", expected)),
                1 => (
                    json!([fence("lease/a", expected)]),
                    json!([fence("lease/b", expected)]),
                ),
                _ => (
                    json!([fence("lease/a", expected), fence("lease/b", 1)]),
                    json!([fence("lease/a", expected), fence("lease/b", expected)]),
                ),
            };
            let result = registry
                .dispatch(
                    "stream.batch",
                    json!({"atomic":atomic,"ops":[
                        {"op":"write","kind":"head","key":"lease/a",
                         "doc":{"renewed":"a"},"expected_version":1},
                        {"op":"append","stream":"mixed","record":"first","fence":first},
                        {"op":"write","kind":"head","key":"lease/b",
                         "doc":{"renewed":"b"},"expected_version":1},
                        {"op":"append","stream":"mixed","record":"last","fence":last}
                    ]}),
                )
                .await
                .unwrap();
            assert_eq!(result["committed"], true);
            let results = result["results"].as_array().unwrap();
            assert_eq!(results.len(), 4);
            assert_eq!(results[0]["id"], a["id"]);
            assert_eq!(results[0]["version"], 2);
            assert_eq!(results[1]["seq"], 1);
            assert_eq!(results[2]["id"], b["id"]);
            assert_eq!(results[2]["version"], 2);
            assert_eq!(results[3]["seq"], 2);
            let read = registry
                .dispatch("stream.read", json!({"stream":"mixed"}))
                .await
                .unwrap();
            assert_eq!(records(&read), vec![json!("first"), json!("last")]);
            assert_eq!(read["head_seq"], 2);
            for (original, renewed) in [(a, "a"), (b, "b")] {
                let after = registry
                    .dispatch("get", json!({"id":original["id"]}))
                    .await
                    .unwrap();
                assert_eq!(after["version"], 2);
                assert_eq!(
                    serde_json::from_str::<Value>(after["content"].as_str().unwrap()).unwrap(),
                    json!({"renewed":renewed})
                );
            }
        }
    }
}

#[tokio::test]
async fn ordered_fences_mixed_batch_later_refusal_obeys_transaction_mode() {
    for atomic in [true, false] {
        for shape in 0..3 {
            let (rt, registry) = mixed_surface();
            let a = lease(&registry, "lease/a").await;
            let b = lease(&registry, "lease/b").await;
            // Checking this atomic fence after member 0 would incorrectly
            // accept version 2; checking per-member fences early accepts 1.
            let expected = if atomic { 2 } else { 1 };
            let (fences, index) = match shape {
                0 => (fence("lease/a", expected), None),
                1 => (json!([fence("lease/a", expected)]), Some("0")),
                _ => (
                    json!([fence("lease/b", 1), fence("lease/a", expected)]),
                    Some("1"),
                ),
            };
            let before = population(&rt).await;
            let outcome = registry
                .dispatch(
                    "stream.batch",
                    json!({"atomic":atomic,"ops":[
                        {"op":"write","kind":"head","key":"lease/a",
                         "doc":{"renewed":true},"expected_version":1},
                        {"op":"append","stream":"mixed-refusal","record":"before",
                         "fence":fence("lease/b", 1)},
                        {"op":"write","kind":"head","key":"candidate","doc":{"created":true}},
                        {"op":"append","stream":"mixed-refusal","record":"refused","fence":fences},
                        {"op":"append","stream":"mixed-refusal","record":"after"}
                    ]}),
                )
                .await;
            let mut details = json!({
                "reason":"fence_conflict",
                "key":"lease/a",
                "expected_version":expected.to_string(),
                "current_version":if atomic { "1" } else { "2" }
            });
            if let Some(index) = index {
                details["index"] = json!(index);
            }
            if atomic {
                details["member"] = json!("3");
                assert_eq!(
                    reason(outcome.unwrap_err(), "fence_conflict")["details"],
                    details
                );
                assert_eq!(population(&rt).await, before);
            } else {
                let result = outcome.unwrap();
                assert_eq!(result["committed"], true);
                let results = result["results"].as_array().unwrap();
                assert_eq!(results.len(), 5);
                assert_eq!(results[0]["id"], a["id"]);
                assert_eq!(results[0]["version"], 2);
                assert_eq!(results[1]["seq"], 1);
                assert_eq!(results[2]["version"], 1);
                assert_eq!(results[3]["kind"], "conflict");
                assert_eq!(results[3]["details"], details);
                assert_eq!(results[3]["domain_disposition"], "not_committed");
                assert_eq!(results[4]["seq"], 2);
            }
            let read = registry
                .dispatch("stream.read", json!({"stream":"mixed-refusal"}))
                .await
                .unwrap();
            assert_eq!(
                records(&read),
                if atomic {
                    vec![]
                } else {
                    vec![json!("before"), json!("after")]
                }
            );
            assert_eq!(read["head_seq"], if atomic { 0 } else { 2 });
            let candidate = registry
                .dispatch("get", json!({"key":"candidate","kind":"head"}))
                .await;
            if atomic {
                assert!(candidate.is_err());
            } else {
                let candidate = candidate.unwrap();
                assert_eq!(candidate["version"], 1);
                assert_eq!(candidate["content"], "{\"created\":true}");
            }
            let after_a = registry
                .dispatch("get", json!({"id":a["id"]}))
                .await
                .unwrap();
            assert_eq!(after_a["version"], if atomic { 1 } else { 2 });
            assert_eq!(
                after_a["content"],
                if atomic { "{}" } else { "{\"renewed\":true}" }
            );
            let after_b = registry
                .dispatch("get", json!({"id":b["id"]}))
                .await
                .unwrap();
            assert_eq!(after_b["version"], b["version"]);
            assert_eq!(after_b["content"], b["content"]);
        }
    }
}
