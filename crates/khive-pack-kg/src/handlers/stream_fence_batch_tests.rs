use super::*;

fn batch_surface() -> (KhiveRuntime, VerbRegistry) {
    let (rt, registry) = super::surface();
    // Match transport boot: a bare runtime deliberately has no kind registry.
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
async fn ordered_fences_batch_commits_objects_and_lists() {
    for atomic in [true, false] {
        let (_, registry) = batch_surface();
        let a = lease(&registry, "lease/a").await;
        let b = lease(&registry, "lease/b").await;
        for fences in [
            fence("lease/a", 1),
            json!([fence("lease/a", 1)]),
            json!([fence("lease/a", 1), fence("lease/b", 1)]),
        ] {
            let result = registry
                .dispatch(
                    "stream.batch",
                    json!({"atomic":atomic,"ops":[
                        {"op":"append","stream":"good","record":1,"fence":fences},
                        {"op":"append","stream":"good","record":2,"fence":fences}
                    ]}),
                )
                .await
                .unwrap();
            assert_eq!(result["committed"], true);
            let results = result["results"].as_array().unwrap();
            assert_eq!(
                results[1]["seq"].as_i64().unwrap(),
                results[0]["seq"].as_i64().unwrap() + 1
            );
            for result in results {
                assert_eq!(
                    registry
                        .dispatch("get", json!({"id":result["id"]}))
                        .await
                        .unwrap()["version"],
                    1
                );
            }
        }
        assert_eq!(
            registry
                .dispatch("stream.stat", json!({"stream":"good"}))
                .await
                .unwrap()["head_seq"],
            6
        );
        for original in [a, b] {
            let after = registry
                .dispatch("get", json!({"id":original["id"]}))
                .await
                .unwrap();
            assert_eq!(after["version"], original["version"]);
            assert_eq!(after["content"], original["content"]);
        }
    }
}

#[tokio::test]
async fn ordered_fences_batch_refusal_placement_and_unchanged_leases() {
    for atomic in [true, false] {
        for (fences, key, current, index) in [
            (fence("lease/a", 2), "lease/a", Some("1"), None),
            (
                json!([fence("lease/a", 2)]),
                "lease/a",
                Some("1"),
                Some("0"),
            ),
            (
                json!([fence("lease/a", 1), fence("lease/b", 2)]),
                "lease/b",
                Some("1"),
                Some("1"),
            ),
            (
                json!([fence("lease/a", 2), fence("lease/b", 2)]),
                "lease/a",
                Some("1"),
                Some("0"),
            ),
            (
                json!([fence("lease/a", 1), fence("missing", 2)]),
                "missing",
                None,
                Some("1"),
            ),
        ] {
            let (rt, registry) = batch_surface();
            let a = lease(&registry, "lease/a").await;
            let b = lease(&registry, "lease/b").await;
            let before = population(&rt).await;
            let result = registry
                .dispatch(
                    "stream.batch",
                    json!({"atomic":atomic,"ops":[
                        {"op":"append","stream":"batch","record":"before"},
                        {"op":"append","stream":"batch","record":"refused","fence":fences},
                        {"op":"append","stream":"batch","record":"after"}
                    ]}),
                )
                .await;
            let mut details = json!({"reason":"fence_conflict","key":key,"expected_version":"2"});
            if let Some(current) = current {
                details["current_version"] = json!(current);
            }
            if let Some(index) = index {
                details["index"] = json!(index);
            }
            if atomic {
                details["member"] = json!("1");
                assert_eq!(
                    reason(result.unwrap_err(), "fence_conflict")["details"],
                    details
                );
                assert_eq!(population(&rt).await, before);
            } else {
                let result = result.unwrap();
                assert_eq!(result["committed"], true);
                assert_eq!(result["results"][1]["details"], details);
                assert_eq!(result["results"][1]["domain_disposition"], "not_committed");
                assert_eq!(result["results"][0]["seq"], 1);
                assert_eq!(result["results"][2]["seq"], 2);
                let entries = registry
                    .dispatch("stream.read", json!({"stream":"batch"}))
                    .await
                    .unwrap();
                assert_eq!(entries["entries"][0]["record"], "before");
                assert_eq!(entries["entries"][1]["record"], "after");
            }
            assert_eq!(
                registry
                    .dispatch("stream.stat", json!({"stream":"batch"}))
                    .await
                    .unwrap()["head_seq"],
                if atomic { 0 } else { 2 }
            );
            for original in [a, b] {
                let after = registry
                    .dispatch("get", json!({"id":original["id"]}))
                    .await
                    .unwrap();
                assert_eq!(after["version"], original["version"]);
                assert_eq!(after["content"], original["content"]);
            }
        }
    }
}

#[tokio::test]
async fn ordered_fences_batch_validates_all_members_before_writing() {
    for atomic in [true, false] {
        for fences in [
            Value::Null,
            json!([]),
            json!([fence("x", 1), fence("x", 2)]),
            json!([fence("x", 0)]),
            json!([null]),
            json!([{"kind":"head","key":"x","expected_version":1,"extra":true}]),
            json!([{"kind":"unregistered-kind","key":"x","expected_version":1}]),
        ] {
            let (rt, registry) = batch_surface();
            let before = population(&rt).await;
            let error = registry
                .dispatch(
                    "stream.batch",
                    json!({"atomic":atomic,"ops":[
                        {"op":"append","stream":"shape","record":1},
                        {"op":"append","stream":"shape","record":2,"fence":fences}
                    ]}),
                )
                .await
                .unwrap_err();
            assert!(matches!(error, RuntimeError::InvalidInput(_)), "{error:?}");
            assert_eq!(population(&rt).await, before);
        }
    }
}
