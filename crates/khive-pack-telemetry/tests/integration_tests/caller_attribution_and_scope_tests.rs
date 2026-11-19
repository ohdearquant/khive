#[tokio::test]
async fn classification_and_caller_attribution_are_visible_in_every_emit() {
    let (registry, _) = registry(config());
    let mut recorded = Vec::new();
    for actor in [None, Some("worker"), Some("forged-worker")] {
        let mut params = json!({"kind":"run.started", "payload":null});
        if let Some(actor) = actor {
            params["actor"] = actor.into();
        }
        let result = registry.dispatch("telemetry.emit", params).await.unwrap();
        assert_eq!(result["actor"], "actor:worker");
        assert_eq!(result["classified"], true);
        assert_eq!(result["outcome"], "recorded");
        assert!(result.get("error").is_none());
        assert!(result.get("dropped").is_none());
        if actor == Some("forged-worker") {
            assert_eq!(result["actor_argument_ignored"], true);
        } else {
            assert!(result.get("actor_argument_ignored").is_none());
        }
        recorded.push(result);
    }
    let page = existing_read(&registry, "events").await;
    for (entry, emitted) in page["entries"].as_array().unwrap().iter().zip(recorded) {
        assert_eq!(entry["id"], emitted["receipt_id"]);
        assert_eq!(entry["seq"], emitted["seq"]);
        assert_eq!(entry["record"]["actor"], "actor:worker");
    }
    let classified = emit(&registry, "turn.delta", Value::Null).await;
    let fallback = emit(&registry, "new.kind", Value::Null).await;
    assert_eq!(classified["classified"], true);
    assert_eq!(fallback["classified"], false);
    for result in [classified, fallback] {
        assert_eq!(result["carrier"], "ephemeral");
        assert_eq!(result["outcome"], "dropped");
        assert!(result["receipt_id"].is_null());
        assert!(result.get("seq").is_none());
        assert!(result.get("error").is_none());
    }
    assert_eq!(
        existing_read(&registry, "events").await["entries"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    let counts = registry.dispatch("telemetry.counts", json!({"stream":"events", "window":{"since":"2000-01-01T00:00:00Z"}, "group_by":["actor"]})).await.unwrap();
    assert_eq!(
        counts["rows"],
        json!([{"key":{"actor":"actor:worker"},"count":3}])
    );
}

#[tokio::test]
async fn server_owned_actor_stamp_cannot_be_sourced_from_arguments_or_payload() {
    let (registry, _) = registry(config());
    for assertion in ["worker", "actor:worker", "foreign-worker"] {
        let payload = json!({"actor":"foreign-worker", "kind":"foreign.kind"});
        let result = registry
            .dispatch(
                "telemetry.emit",
                json!({
                    "kind":"run.started", "actor":assertion, "payload":payload,
                }),
            )
            .await
            .unwrap();
        assert_eq!(result["actor"], "actor:worker");
        assert_eq!(
            result
                .get("actor_argument_ignored")
                .and_then(Value::as_bool),
            (assertion == "foreign-worker").then_some(true)
        );
    }
    let page = existing_read(&registry, "events").await;
    let entries = page["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 3);
    for entry in entries {
        assert_eq!(entry["record"]["actor"], "actor:worker");
        assert_eq!(entry["record"]["kind"], "run.started");
        assert_eq!(entry["record"]["payload"]["actor"], "foreign-worker");
    }
    let counts = registry
        .dispatch(
            "telemetry.counts",
            json!({
                "stream":"events", "window":{"since":"2000-01-01T00:00:00Z"}, "group_by":["actor"],
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        counts["rows"],
        json!([{"key":{"actor":"actor:worker"}, "count":3}])
    );
}

#[tokio::test]
async fn read_and_counts_scope_to_caller_and_gate_foreign_and_all_actor_requests() {
    for fleet in [false, true] {
        let readers: &[&str] = if fleet { &["worker"] } else { &[] };
        let (registry, runtime) =
            registry_with_scope(config(), "worker", &["visible-peer"], readers);
        for actor in [
            "actor:worker",
            "worker",
            "actor:visible-peer",
            "actor:hidden-peer",
        ] {
            append_at(
                &registry,
                &runtime,
                json!({"kind":"run.started", "actor":actor}),
                "2026-01-02T00:00:00Z",
            )
            .await;
        }
        for verb in ["telemetry.read", "telemetry.counts"] {
            let base = json!({"stream":"other-stream"});
            let mut params = base;
            if verb == "telemetry.counts" {
                params["window"] = json!({"since":"2000-01-01T00:00:00Z"});
            }
            let result = registry.dispatch(verb, params.clone()).await.unwrap();
            let count = |v: &Value| {
                if verb == "telemetry.read" {
                    v["events"].as_array().unwrap().len() as u64
                } else {
                    v["total"].as_u64().unwrap()
                }
            };
            assert_eq!(
                count(&result),
                2,
                "{verb} default must include only caller rows"
            );
            params["actor"] = "visible-peer".into();
            assert_eq!(
                count(&registry.dispatch(verb, params.clone()).await.unwrap()),
                1
            );
            params["actor"] = "hidden-peer".into();
            assert!(registry
                .dispatch(verb, params.clone())
                .await
                .unwrap_err()
                .to_string()
                .contains("not visible"));
            params.as_object_mut().unwrap().remove("actor");
            params["all_actors"] = true.into();
            let all = registry.dispatch(verb, params.clone()).await;
            if fleet {
                assert_eq!(count(&all.unwrap()), 4);
            } else {
                assert!(all.unwrap_err().to_string().contains("fleet reader"));
            }
            params["actor"] = "worker".into();
            assert!(registry
                .dispatch(verb, params)
                .await
                .unwrap_err()
                .to_string()
                .contains("cannot be combined"));
        }
    }
}

#[tokio::test]
async fn explicit_stamped_actor_is_not_coalesced_with_another_principal() {
    let (registry, runtime) = registry_with_scope(config(), "actor:peer", &["peer"], &[]);
    for actor in ["actor:actor:peer", "actor:peer", "peer"] {
        append_at(
            &registry,
            &runtime,
            json!({"kind":"run.started", "actor":actor}),
            "2026-01-02T00:00:00Z",
        )
        .await;
    }
    let own = registry
        .dispatch("telemetry.read", json!({"stream":"other-stream"}))
        .await
        .unwrap();
    assert_eq!(own["events"].as_array().unwrap().len(), 1);
    assert_eq!(own["events"][0]["record"]["actor"], "actor:actor:peer");
    let explicit = registry
        .dispatch(
            "telemetry.read",
            json!({"stream":"other-stream", "actor":"actor:peer"}),
        )
        .await
        .unwrap();
    assert_eq!(explicit["events"].as_array().unwrap().len(), 1);
    assert_eq!(explicit["events"][0]["record"]["actor"], "actor:peer");
}
