// ── exclude_tags and source_id on recall hits ────────────────────────────────

/// `exclude_tags` drops memories whose stored tags include any excluded value,
/// after `tags`/`tag_mode` and before ranking and `limit`; untagged memories
/// and memories tagged only with other values survive.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn recall_exclude_tags_drops_own_run_writes_and_keeps_the_rest() {
    let registry = make_registry(make_runtime());
    let mut ids = Vec::new();
    for (content, tags) in [
        (
            "exclusion regression shared target one",
            json!(["run:a", "shared"]),
        ),
        (
            "exclusion regression shared target two",
            json!(["run:b", "shared"]),
        ),
        ("exclusion regression shared target three", Value::Null),
    ] {
        let mut args = json!({"content": content, "salience": 0.9});
        if !tags.is_null() {
            args["tags"] = tags;
        }
        let stored = registry
            .dispatch("memory.remember", args)
            .await
            .expect("remember");
        ids.push(stored["id"].as_str().unwrap().to_owned());
    }
    let recall = |args: Value| {
        let registry = &registry;
        async move {
            let hits = registry
                .dispatch("memory.recall", args)
                .await
                .expect("recall");
            hits.as_array()
                .unwrap()
                .iter()
                .filter_map(|h| h["id"].as_str().map(str::to_owned))
                .collect::<Vec<_>>()
        }
    };

    let all = recall(json!({"query": "exclusion regression shared target", "limit": 20})).await;
    for id in &ids {
        assert!(all.contains(id), "no-filter recall must return {id}");
    }

    let excluded = recall(json!({
        "query": "exclusion regression shared target",
        "limit": 20,
        "exclude_tags": ["run:a"]
    }))
    .await;
    assert!(!excluded.contains(&ids[0]), "run:a memory must be excluded");
    assert!(excluded.contains(&ids[1]), "run:b memory must survive");
    assert!(excluded.contains(&ids[2]), "untagged memory must survive");

    // Combined with an inclusive filter: tags narrows to "shared", exclude drops run:b.
    let combined = recall(json!({
        "query": "exclusion regression shared target",
        "limit": 20,
        "tags": ["shared"],
        "exclude_tags": ["run:b"]
    }))
    .await;
    assert_eq!(
        combined,
        vec![ids[0].clone()],
        "only run:a carries shared and is not excluded"
    );

    // Empty list is a no-op, not an error.
    let empty = recall(
        json!({"query": "exclusion regression shared target", "limit": 20, "exclude_tags": []}),
    )
    .await;
    assert_eq!(empty.len(), all.len());
}

/// `include_source_id=true` puts the annotates target on every hit and null
/// where a memory has no source; the default response shape is unchanged.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn recall_include_source_id_reads_the_annotates_edge() {
    let registry = make_registry(make_runtime());
    let source = registry
        .dispatch(
            "create",
            json!({"kind": "person", "name": "Provenance Source", "description": "source entity"}),
        )
        .await
        .expect("create source entity");
    let source_id = source["id"].as_str().unwrap().to_owned();

    let with_source = registry
        .dispatch(
            "memory.remember",
            json!({"content": "provenance regression shared target alpha", "salience": 0.9, "source_id": source_id}),
        )
        .await
        .expect("remember with source");
    let with_source_id = with_source["id"].as_str().unwrap().to_owned();
    let without = registry
        .dispatch(
            "memory.remember",
            json!({"content": "provenance regression shared target beta", "salience": 0.9}),
        )
        .await
        .expect("remember without source");
    let without_id = without["id"].as_str().unwrap().to_owned();

    let plain = registry
        .dispatch(
            "memory.recall",
            json!({"query": "provenance regression shared target", "limit": 20}),
        )
        .await
        .expect("recall default");
    for hit in plain.as_array().unwrap() {
        assert!(
            hit.get("source_id").is_none(),
            "default shape carries no source_id; got {hit}"
        );
    }

    let hits = registry
        .dispatch(
            "memory.recall",
            json!({"query": "provenance regression shared target", "limit": 20, "include_source_id": true}),
        )
        .await
        .expect("recall with source ids");
    let by_id: std::collections::HashMap<String, Value> = hits
        .as_array()
        .unwrap()
        .iter()
        .map(|h| (h["id"].as_str().unwrap().to_owned(), h["source_id"].clone()))
        .collect();
    assert_eq!(
        by_id.get(&with_source_id),
        Some(&json!(source_id)),
        "sourced memory carries its source"
    );
    assert_eq!(
        by_id.get(&without_id),
        Some(&Value::Null),
        "unsourced memory carries null, not absence"
    );
}
