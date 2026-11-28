// ── Tag-filtered recall ───────────────────────────────────────────────────────

/// Tag filter — OR (any), AND (all), and no-filter behaviors.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn recall_tags_filter_any_all_and_no_filter() {
    let registry = make_registry(make_runtime());

    // Store three memories with distinct tag combos.
    let impl_khive = registry
        .dispatch(
            "memory.remember",
            json!({
                "content": "tag filter regression shared semantic target alpha",
                "salience": 0.9,
                "tags": ["role:implementer", "khive"]
            }),
        )
        .await
        .expect("remember impl khive");
    let impl_khive_id = impl_khive["id"].as_str().unwrap().to_owned();

    let critic_khive = registry
        .dispatch(
            "memory.remember",
            json!({
                "content": "tag filter regression shared semantic target beta",
                "salience": 0.9,
                "tags": ["role:critic", "khive"]
            }),
        )
        .await
        .expect("remember critic khive");
    let critic_khive_id = critic_khive["id"].as_str().unwrap().to_owned();

    let impl_rust = registry
        .dispatch(
            "memory.remember",
            json!({
                "content": "tag filter regression shared semantic target gamma",
                "salience": 0.9,
                "tags": ["role:implementer", "rust"]
            }),
        )
        .await
        .expect("remember impl rust");
    let impl_rust_id = impl_rust["id"].as_str().unwrap().to_owned();

    // no-filter: all three should appear.
    let no_filter = registry
        .dispatch(
            "memory.recall",
            json!({ "query": "tag filter regression shared semantic target", "limit": 20 }),
        )
        .await
        .expect("recall no filter");
    let no_filter_ids: Vec<&str> = no_filter
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|h| h["id"].as_str())
        .collect();
    assert!(
        no_filter_ids.contains(&impl_khive_id.as_str()),
        "no-filter must return impl+khive memory"
    );
    assert!(
        no_filter_ids.contains(&critic_khive_id.as_str()),
        "no-filter must return critic+khive memory"
    );
    assert!(
        no_filter_ids.contains(&impl_rust_id.as_str()),
        "no-filter must return impl+rust memory"
    );

    // any (OR): tags=["role:critic", "rust"] → critic_khive and impl_rust, not impl_khive.
    let any_result = registry
        .dispatch(
            "memory.recall",
            json!({
                "query": "tag filter regression shared semantic target",
                "limit": 20,
                "tags": ["role:critic", "rust"],
                "tag_mode": "any"
            }),
        )
        .await
        .expect("recall tag any");
    let any_ids: Vec<&str> = any_result
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|h| h["id"].as_str())
        .collect();
    assert!(
        any_ids.contains(&critic_khive_id.as_str()),
        "any filter must include critic+khive (has role:critic)"
    );
    assert!(
        any_ids.contains(&impl_rust_id.as_str()),
        "any filter must include impl+rust (has rust)"
    );
    assert!(
        !any_ids.contains(&impl_khive_id.as_str()),
        "any filter must exclude impl+khive (has neither role:critic nor rust)"
    );

    // all (AND): tags=["role:implementer", "khive"] → impl_khive only.
    let all_result = registry
        .dispatch(
            "memory.recall",
            json!({
                "query": "tag filter regression shared semantic target",
                "limit": 20,
                "tags": ["role:implementer", "khive"],
                "tag_mode": "all"
            }),
        )
        .await
        .expect("recall tag all");
    let all_ids: Vec<&str> = all_result
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|h| h["id"].as_str())
        .collect();
    assert!(
        all_ids.contains(&impl_khive_id.as_str()),
        "all filter must include impl+khive (has both role:implementer and khive)"
    );
    assert!(
        !all_ids.contains(&critic_khive_id.as_str()),
        "all filter must exclude critic+khive (missing role:implementer)"
    );
    assert!(
        !all_ids.contains(&impl_rust_id.as_str()),
        "all filter must exclude impl+rust (missing khive)"
    );
}

/// B7: raw_score must be present (possibly null) in every result returned by
/// memory.recall, including when tag filters narrow the result set with tag_mode="all".
/// The field is null for text-only hits (no vector index) and a float for vector hits.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn recall_raw_score_field_always_present_with_tag_filter() {
    let registry = make_registry(make_runtime());

    registry
        .dispatch(
            "memory.remember",
            json!({
                "content": "raw score presence check alpha beta gamma delta epsilon",
                "salience": 0.9,
                "tags": ["team:alpha", "project:khive"]
            }),
        )
        .await
        .expect("remember tagged memory");

    registry
        .dispatch(
            "memory.remember",
            json!({
                "content": "raw score presence check alpha beta gamma delta epsilon",
                "salience": 0.9,
                "tags": ["team:alpha", "project:khive"]
            }),
        )
        .await
        .expect("remember second tagged memory");

    for tag_mode in &["any", "all"] {
        let result = registry
            .dispatch(
                "memory.recall",
                json!({
                    "query": "raw score presence check alpha beta gamma",
                    "limit": 20,
                    "tags": ["team:alpha", "project:khive"],
                    "tag_mode": tag_mode
                }),
            )
            .await
            .unwrap_or_else(|e| panic!("recall tag_mode={tag_mode} failed: {e}"));

        let hits = result.as_array().expect("results must be an array");
        assert!(
            !hits.is_empty(),
            "tag_mode={tag_mode}: expected at least one result"
        );
        for (i, hit) in hits.iter().enumerate() {
            let obj = hit.as_object().expect("each hit must be a JSON object");
            assert!(
                obj.contains_key("raw_score"),
                "tag_mode={tag_mode} result[{i}] missing raw_score field; got keys: {:?}",
                obj.keys().collect::<Vec<_>>()
            );
        }
    }
}

/// Metadata: memory.recall handler must advertise tags and tag_mode params.
#[test]
fn recall_handler_metadata_advertises_tags_and_tag_mode() {
    let recall_def = khive_pack_memory::MemoryPack::HANDLERS
        .iter()
        .find(|h| h.name == "memory.recall")
        .expect("memory.recall handler must be registered");

    let param_names: Vec<&str> = recall_def.params.iter().map(|p| p.name).collect();
    assert!(
        param_names.contains(&"tags"),
        "memory.recall must advertise 'tags' param; got: {param_names:?}"
    );
    assert!(
        param_names.contains(&"tag_mode"),
        "memory.recall must advertise 'tag_mode' param; got: {param_names:?}"
    );
    assert!(
        param_names.contains(&"prefer_tags"),
        "memory.recall must advertise 'prefer_tags' param; got: {param_names:?}"
    );
}

/// `prefer_tags` ranks memories carrying any preferred tag ahead of the rest
/// without dropping anything: other matches fill the remaining slots, and each
/// group keeps its score order.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn recall_prefer_tags_ranks_tagged_hits_first_and_fills_with_the_rest() {
    let registry = make_registry(make_runtime());
    let mut preferred = Vec::new();
    let mut others = Vec::new();
    for (content, salience, tags) in [
        ("preference regression shared target one", 0.95, json!(["other"])),
        ("preference regression shared target two", 0.9, Value::Null),
        (
            "preference regression shared target three",
            0.4,
            json!(["pref", "other"]),
        ),
        ("preference regression shared target four", 0.35, json!(["pref"])),
    ] {
        let is_preferred = tags
            .as_array()
            .is_some_and(|tags| tags.iter().any(|tag| tag == "pref"));
        let mut args = json!({"content": content, "salience": salience});
        if !tags.is_null() {
            args["tags"] = tags;
        }
        let stored = registry
            .dispatch("memory.remember", args)
            .await
            .expect("remember");
        let id = stored["id"].as_str().unwrap().to_owned();
        if is_preferred {
            preferred.push(id);
        } else {
            others.push(id);
        }
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
    let query = "preference regression shared target";

    let default_order = recall(json!({"query": query, "limit": 20})).await;
    let position = |order: &[String], id: &str| {
        order
            .iter()
            .position(|hit| hit == id)
            .unwrap_or_else(|| panic!("{id} missing from {order:?}"))
    };
    // Precondition: without a preference the higher-salience memories lead.
    for other in &others {
        for tagged in &preferred {
            assert!(
                position(&default_order, other) < position(&default_order, tagged),
                "default order must rank {other} above {tagged}: {default_order:?}"
            );
        }
    }

    let expected: Vec<String> = default_order
        .iter()
        .filter(|id| preferred.contains(id))
        .chain(default_order.iter().filter(|id| !preferred.contains(id)))
        .cloned()
        .collect();
    let preferred_order =
        recall(json!({"query": query, "limit": 20, "prefer_tags": ["pref"]})).await;
    assert_eq!(
        preferred_order, expected,
        "preferred hits first, the rest after them, each in score order, nothing dropped"
    );

    // The preference applies before limit, so tagged hits below the cut still lead
    // and other matches fill the remaining slot.
    let limited = recall(json!({"query": query, "limit": 3, "prefer_tags": ["pref"]})).await;
    assert_eq!(limited, expected[..3].to_vec());

    // An empty list is a no-op, not an error.
    let empty = recall(json!({"query": query, "limit": 20, "prefer_tags": []})).await;
    assert_eq!(empty, default_order);
}
