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
async fn absence_fences_batch_wide_match_aliases_and_preserve_refusal_details() {
    for state in ["missing", "live", "soft_deleted"] {
        let mut outcomes = Vec::new();
        for field in ["expected_version", "version"] {
            let (rt, registry) = batch_surface();
            absence_fence_subject(&registry, state).await;
            let before = population(&rt).await;
            let result = registry
                .dispatch(
                    "stream.batch",
                    json!({
                        "fence":fence_version_field("lease/absence", field, Value::Null),
                        "ops":[
                            {"op":"append", "stream":"absence-batch", "record":"first"},
                            {"op":"append", "stream":"absence-batch", "record":"second"}
                        ]
                    }),
                )
                .await;
            if state == "live" {
                let error = reason(result.unwrap_err(), "fence_conflict");
                assert_eq!(
                    error["details"],
                    json!({"reason":"fence_conflict", "key":"lease/absence", "expected_version":"absent", "current_version":"3"})
                );
                assert_eq!(population(&rt).await, before);
                assert_eq!(heads(&registry, &["absence-batch"]).await, vec![0]);
                outcomes.push(error["details"].clone());
            } else {
                let result = result.unwrap_or_else(|error| panic!("{field} {state}: {error}"));
                assert_eq!(result["committed"], true);
                assert_eq!(seqs(&result), vec![1, 2]);
                let read = registry
                    .dispatch("stream.read", json!({"stream":"absence-batch"}))
                    .await
                    .unwrap();
                let records: Vec<_> = read["entries"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|entry| entry["record"].clone())
                    .collect();
                assert_eq!(records, vec![json!("first"), json!("second")]);
                outcomes.push(json!({"seqs":seqs(&result), "records":records}));
            }
        }
        assert_eq!(outcomes[0], outcomes[1], "canonical and alias: {state}");
    }
}

#[tokio::test]
async fn absence_fences_batch_members_inherit_aliases_and_transaction_placement() {
    for atomic in [true, false] {
        for listed in [false, true] {
            for state in ["missing", "live", "soft_deleted"] {
                let mut outcomes = Vec::new();
                for field in ["expected_version", "version"] {
                    let (rt, registry) = batch_surface();
                    lease(&registry, "lease/guard").await;
                    absence_fence_subject(&registry, state).await;
                    let member = fence_version_field("lease/absence", field, Value::Null);
                    let fences = if listed {
                        json!([fence("lease/guard", 1), member])
                    } else {
                        member
                    };
                    let before = population(&rt).await;
                    let result = registry.dispatch("stream.batch", json!({"atomic":atomic, "ops":[
                        {"op":"append", "stream":"absence-member", "record":"before"},
                        {"op":"append", "stream":"absence-member", "record":"guarded", "fence":fences},
                        {"op":"append", "stream":"absence-member", "record":"after"}
                    ]})).await;
                    if state == "live" {
                        let mut details = json!({"reason":"fence_conflict", "key":"lease/absence", "expected_version":"absent", "current_version":"3"});
                        if listed {
                            details["index"] = json!("1");
                        }
                        let actual = if atomic {
                            details["member"] = json!("1");
                            let error = reason(result.unwrap_err(), "fence_conflict");
                            assert_eq!(population(&rt).await, before);
                            assert_eq!(heads(&registry, &["absence-member"]).await, vec![0]);
                            error["details"].clone()
                        } else {
                            let result = result.unwrap();
                            assert_eq!(result["committed"], true);
                            assert_eq!(result["results"][1]["domain_disposition"], "not_committed");
                            assert_eq!(result["results"][0]["seq"], 1);
                            assert_eq!(result["results"][2]["seq"], 2);
                            let read = registry
                                .dispatch("stream.read", json!({"stream":"absence-member"}))
                                .await
                                .unwrap();
                            assert_eq!(read["entries"].as_array().unwrap().len(), 2);
                            assert_eq!(read["entries"][0]["record"], "before");
                            assert_eq!(read["entries"][1]["record"], "after");
                            result["results"][1]["details"].clone()
                        };
                        assert_eq!(actual, details, "{field} atomic={atomic} list={listed}");
                        outcomes.push(actual);
                    } else {
                        let result = result.unwrap_or_else(|error| {
                            panic!("{field} {state} atomic={atomic} list={listed}: {error}")
                        });
                        assert_eq!(result["committed"], true);
                        assert_eq!(seqs(&result), vec![1, 2, 3]);
                        let read = registry
                            .dispatch("stream.read", json!({"stream":"absence-member"}))
                            .await
                            .unwrap();
                        let records: Vec<_> = read["entries"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|entry| entry["record"].clone())
                            .collect();
                        assert_eq!(
                            records,
                            vec![json!("before"), json!("guarded"), json!("after")]
                        );
                        outcomes.push(json!({"seqs":seqs(&result), "records":records}));
                    }
                }
                assert_eq!(
                    outcomes[0], outcomes[1],
                    "canonical and alias: {state} atomic={atomic} list={listed}"
                );
            }
        }
    }
}

#[tokio::test]
async fn absence_fence_parameter_errors_refuse_whole_batches_before_members() {
    for (top_level, atomic) in [(true, true), (false, true), (false, false)] {
        for member in [
            json!({"key":"lease/absence", "kind":"head"}),
            json!({"key":"lease/absence", "kind":"head", "expected_version":null, "version":null}),
            json!({"key":"lease/absence", "kind":"head", "version":null, "extra":true}),
            fence_version_field("lease/absence", "version", json!("1")),
            fence_version_field("lease/absence", "version", json!(0)),
            fence_version_field("lease/absence", "expected_version", json!(-1)),
        ] {
            for listed in [false, true] {
                if top_level && listed {
                    continue;
                }
                let (rt, registry) = batch_surface();
                let before = population(&rt).await;
                let fences = if listed {
                    json!([member])
                } else {
                    member.clone()
                };
                let mut args = json!({"atomic":atomic, "ops":[
                    {"op":"append", "stream":"absence-invalid", "record":"before"},
                    {"op":"append", "stream":"absence-invalid", "record":"invalid"}
                ]});
                if top_level {
                    args["fence"] = fences;
                } else {
                    args["ops"][1]["fence"] = fences;
                }
                let error = registry.dispatch("stream.batch", args).await.unwrap_err();
                let RuntimeError::InvalidInput(message) = error else {
                    panic!("parameter refusal: {error:?}")
                };
                assert!(message.contains("fence"), "{message}");
                assert!(
                    !message.contains("Shape") && !message.contains("untagged enum"),
                    "{message}"
                );
                if member.get("expected_version").is_none() && member.get("version").is_none() {
                    assert!(
                        message
                            .contains("fence requires expected_version (positive integer or null)"),
                        "{message}"
                    );
                }
                assert_eq!(population(&rt).await, before);
                assert_eq!(heads(&registry, &["absence-invalid"]).await, vec![0]);
            }
        }
    }
}

#[tokio::test]
async fn batch_null_fence_and_observed_version_spelling_remain_distinct() {
    let (rt, registry) = batch_surface();
    lease(&registry, "lease/absence").await;
    let result = registry
        .dispatch(
            "stream.batch",
            json!({"fence":null, "ops":[
                {"op":"append", "stream":"null-fence", "record":"before"},
                {"op":"append", "stream":"null-fence", "record":"refused", "expected_seq":9},
                {"op":"append", "stream":"null-fence", "record":"after"}
            ]}),
        )
        .await
        .unwrap();
    assert_eq!(result["results"][0]["seq"], 1);
    assert_eq!(result["results"][1]["details"]["reason"], "seq_conflict");
    assert_eq!(result["results"][2]["seq"], 2);
    let before = population(&rt).await;
    for observed in [
        json!({"key":"missing", "kind":"head"}),
        json!({"key":"missing", "kind":"head", "expected_version":null}),
        json!({"key":"missing", "kind":"head", "version":null, "expected_version":null}),
    ] {
        let error = registry
            .dispatch(
                "stream.batch",
                json!({"atomic":true, "observed":[observed], "ops":[
                    {"op":"append", "stream":"observed-invalid", "record":true}
                ]}),
            )
            .await
            .unwrap_err();
        let RuntimeError::InvalidInput(message) = error else {
            panic!("observed parameter refusal: {error:?}")
        };
        if observed.get("version").is_none() {
            assert!(
                message.contains("observed entry 0 requires version (positive integer or null)"),
                "{message}"
            );
        } else {
            assert!(
                message.contains("unknown field") && message.contains("expected_version"),
                "{message}"
            );
        }
        assert_eq!(population(&rt).await, before);
    }
    assert_eq!(heads(&registry, &["observed-invalid"]).await, vec![0]);
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

#[tokio::test]
async fn ordered_fences_cap_accepts_exactly_100_on_every_write_surface() {
    let (_, registry) = batch_surface();
    let mut fences = Vec::new();
    for index in 0..100 {
        let key = format!("fence-cap/{index}");
        lease(&registry, &key).await;
        fences.push(fence(&key, 1));
    }
    let target = registry
        .dispatch(
            "create",
            json!({"kind":"head","content":"{}","fence":fences}),
        )
        .await
        .unwrap();
    registry
        .dispatch(
            "update",
            json!({"id":target["id"],"content":"{\"updated\":true}","fence":fences}),
        )
        .await
        .unwrap();
    registry
        .dispatch(
            "stream.append",
            json!({"stream":"fence-cap","record":1,"fence":fences}),
        )
        .await
        .unwrap();
    for atomic in [true, false] {
        let result = registry
            .dispatch(
                "stream.batch",
                json!({"atomic":atomic,"ops":[
                    {"op":"append","stream":"fence-cap","record":2,"fence":fences}
                ]}),
            )
            .await
            .unwrap();
        assert_eq!(result["committed"], true);
    }
    assert_eq!(heads(&registry, &["fence-cap"]).await, vec![3]);
    let target = registry
        .dispatch("get", json!({"id":target["id"]}))
        .await
        .unwrap();
    assert_eq!(target["version"], 2);
}

#[tokio::test]
async fn ordered_fences_cap_refuses_before_entry_interpretation_and_writer_admission() {
    let (rt, registry) = batch_surface();
    let target = lease(&registry, "target").await;
    let distinct: Vec<Value> = (0..101)
        .map(|index| fence(&format!("fence-cap/{index}"), 1))
        .collect();
    for fences in [
        json!(distinct),
        json!(vec![Value::Null; 101]),
        json!(vec![fence("duplicate", 0); 101]),
    ] {
        let before = population(&rt).await;
        let before_writers = rt.backend().pool().writer_acquisition_snapshot();
        for (verb, args) in [
            (
                "create",
                json!({"kind":"head","content":"{}","fence":fences}),
            ),
            (
                "update",
                json!({"id":target["id"],"content":"{}","fence":fences}),
            ),
            (
                "stream.append",
                json!({"stream":"fence-cap","record":1,"fence":fences}),
            ),
            (
                "stream.batch",
                json!({"atomic":true,"ops":[
                    {"op":"append","stream":"fence-cap","record":1},
                    {"op":"append","stream":"fence-cap","record":2,"fence":fences}
                ]}),
            ),
            (
                "stream.batch",
                json!({"atomic":false,"ops":[
                    {"op":"append","stream":"fence-cap","record":1},
                    {"op":"append","stream":"fence-cap","record":2,"fence":fences}
                ]}),
            ),
        ] {
            let error = registry.dispatch(verb, args).await.unwrap_err();
            let RuntimeError::InvalidInput(message) = error else {
                panic!("{verb} must refuse invalid_input: {error}");
            };
            assert!(message.contains("at most 100 entries"), "{verb}: {message}");
            assert!(message.contains("sent 101"), "{verb}: {message}");
            assert_eq!(population(&rt).await, before, "{verb}");
            assert_eq!(
                rt.backend().pool().writer_acquisition_snapshot(),
                before_writers,
                "{verb} must refuse before writer admission"
            );
        }
    }
    assert_eq!(heads(&registry, &["fence-cap"]).await, vec![0]);
    let after = registry
        .dispatch("get", json!({"id":target["id"]}))
        .await
        .unwrap();
    assert_eq!(after["version"], target["version"]);
    assert_eq!(after["content"], target["content"]);
}
