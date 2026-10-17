#[tokio::test]
async fn arbitrary_stream_rollup_groups_and_excludes_existing_rows_at_both_boundaries() {
    let (registry, runtime) = registry_with_scope(config(), "worker", &[], &["worker"]);
    for (at, kind, actor, verb) in [
        ("2026-01-01T00:00:00Z", "tool", "a", "old"),
        ("2026-01-02T00:00:00Z", "tool", "a", "read"),
        ("2026-01-02T12:00:00Z", "tool", "a", "read"),
        ("2026-01-02T18:00:00Z", "tool", "b", "write"),
        ("2026-01-03T00:00:00Z", "tool", "a", "new"),
    ] {
        append_at(
            &registry,
            &runtime,
            json!({"kind":kind,"actor":actor,"payload":{"verb":verb}}),
            at,
        )
        .await;
    }
    let result = registry
        .dispatch(
            "telemetry.counts",
            json!({
                "stream":"other-stream", "all_actors":true,
                "window":{"since":"2026-01-02T00:00:00Z","until":"2026-01-03T00:00:00Z"},
                "group_by":["kind","actor","payload.verb"],
            }),
        )
        .await
        .unwrap();
    assert_eq!(result["total"], 3);
    assert_eq!(result["scanned"], 5);
    assert_eq!(result["head_seq"], 5);
    assert_eq!(
        result["rows"],
        json!([
            {"key":{"kind":"tool","actor":"a","payload.verb":"read"},"count":2},
            {"key":{"kind":"tool","actor":"b","payload.verb":"write"},"count":1},
        ])
    );
    assert_eq!(
        existing_read(&registry, "other-stream").await["entries"]
            .as_array()
            .unwrap()
            .len(),
        5
    );
    assert!(existing_read(&registry, "events").await["entries"]
        .as_array()
        .unwrap()
        .is_empty());
    let ungrouped = registry
        .dispatch(
            "telemetry.counts",
            json!({
                "stream":"other-stream", "all_actors":true,
                "window":{"since":"2026-01-01T00:00:00Z","until":"2026-01-04T00:00:00Z"}
            }),
        )
        .await
        .unwrap();
    assert_eq!(ungrouped["total"], 5);
    assert_eq!(
        ungrouped["rows"],
        json!([{"key":{"kind":"tool"},"count":5}])
    );
    let excluded = registry
        .dispatch(
            "telemetry.counts",
            json!({
                "stream":"other-stream", "all_actors":true, "kinds":["absent"],
                "window":{"since":"2026-01-01T00:00:00Z","until":"2026-01-04T00:00:00Z"}
            }),
        )
        .await
        .unwrap();
    assert_eq!(excluded["total"], 0);
    assert_eq!(excluded["scanned"], 5);
}

#[tokio::test]
async fn read_cursor_advances_across_filtered_rows_and_resumes_at_tail() {
    let (registry, _) = registry(config());
    emit(&registry, "run.started", Value::Null).await;
    emit(&registry, "run.started", Value::Null).await;
    let excluded = registry
        .dispatch(
            "telemetry.read",
            json!({
                "stream":"events", "limit":1, "kinds":["run.completed"]
            }),
        )
        .await
        .unwrap();
    assert_eq!(excluded["events"], json!([]));
    assert_eq!(excluded["next_cursor"], 1);
    assert_eq!(excluded["has_more"], true);
    let tail = registry
        .dispatch(
            "telemetry.read",
            json!({
                "stream":"events", "since":excluded["next_cursor"], "kinds":["run.completed"]
            }),
        )
        .await
        .unwrap();
    assert_eq!(tail["events"], json!([]));
    assert_eq!(tail["next_cursor"], 2);
    assert_eq!(tail["has_more"], false);
    assert!(tail.get("gap").is_none());
    emit(&registry, "run.completed", Value::Null).await;
    let resumed = registry
        .dispatch(
            "telemetry.read",
            json!({
                "stream":"events", "since":tail["next_cursor"], "kinds":["run.completed"]
            }),
        )
        .await
        .unwrap();
    assert_eq!(resumed["events"].as_array().unwrap().len(), 1);
    assert_eq!(resumed["next_cursor"], 3);
    assert_eq!(resumed["ephemeral_retention"], "none");
}

#[tokio::test]
async fn counts_default_until_is_now_and_missing_dimensions_are_null() {
    let (registry, runtime) = registry(config());
    append_at(
        &registry,
        &runtime,
        json!({"kind":"tool"}),
        "2000-01-01T00:00:00Z",
    )
    .await;
    let before = Utc::now();
    let result = registry
        .dispatch(
            "telemetry.counts",
            json!({
                "stream":"other-stream", "window":{"since":"1999-01-01T00:00:00Z"},
                "group_by":["payload.verb"],
            }),
        )
        .await
        .unwrap();
    let until = DateTime::parse_from_rfc3339(result["window"]["until"].as_str().unwrap())
        .unwrap()
        .with_timezone(&Utc);
    assert!(until >= before && until <= Utc::now());
    assert_eq!(
        result["rows"],
        json!([{"key":{"payload.verb":null},"count":1}])
    );
}

#[tokio::test]
async fn counts_reads_more_than_one_stream_page() {
    let (registry, _) = registry(config());
    let ops: Vec<_> = (0..1_000)
        .map(|_| json!({"op":"append", "stream":"paged", "record":{"kind":"first","actor":"worker"}, "embed":false}))
        .collect();
    registry
        .dispatch("stream.batch", json!({"ops":ops,"atomic":true}))
        .await
        .expect("seed first full page");
    registry
        .dispatch(
            "stream.append",
            json!({"stream":"paged", "record":{"kind":"last","actor":"worker"}, "embed":false}),
        )
        .await
        .expect("seed second page");
    let result = registry
        .dispatch(
            "telemetry.counts",
            json!({
                "stream":"paged",
                "window":{"since":"2000-01-01T00:00:00Z", "until":"2100-01-01T00:00:00Z"}
            }),
        )
        .await
        .unwrap();
    assert_eq!(result["total"], 1_001);
    assert_eq!(result["scanned"], 1_001);
    assert_eq!(result["head_seq"], 1_001);
    assert_eq!(
        result["rows"],
        json!([
            {"key":{"kind":"first"}, "count":1_000},
            {"key":{"kind":"last"}, "count":1},
        ])
    );
}
