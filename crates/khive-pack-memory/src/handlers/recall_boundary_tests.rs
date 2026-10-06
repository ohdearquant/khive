use super::*;

/// `limit=0` returns no hits, the same as `top_k=0`; a lower clamp of one
/// used to turn it into a single hit.
#[tokio::test]
#[serial(background_tasks)]
#[serial_test::serial(config_ledger)]
async fn recall_limit_zero_returns_no_hits_like_top_k_zero() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let ns = Namespace::parse("local").expect("local namespace");
    let token = rt.authorize(ns).expect("authorize local");
    for i in 0..3 {
        rt.create_note(
            &token,
            "memory",
            None,
            &format!("limit zero probe note {i}"),
            Some(0.7),
            None,
            vec![],
        )
        .await
        .expect("create note");
    }

    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(MemoryPack::new(
        crate::test_support::with_receipt_credentials(rt.clone()),
    ));
    let registry = builder.build().expect("registry");

    let hits_for = |params: serde_json::Value| {
        let registry = &registry;
        async move {
            let out = registry
                .dispatch("memory.recall", params)
                .await
                .expect("recall dispatch");
            match out {
                serde_json::Value::Array(items) => items.len(),
                serde_json::Value::Object(map) => map
                    .get("results")
                    .and_then(serde_json::Value::as_array)
                    .map(Vec::len)
                    .unwrap_or(0),
                _ => panic!("unexpected recall shape"),
            }
        }
    };

    let control = hits_for(serde_json::json!({"query": "limit zero probe note", "limit": 2})).await;
    assert_eq!(
        control, 2,
        "limit=2 is the control and must return two hits"
    );
    let by_top_k =
        hits_for(serde_json::json!({"query": "limit zero probe note", "top_k": 0})).await;
    assert_eq!(by_top_k, 0, "top_k=0 returns no hits");
    let by_limit =
        hits_for(serde_json::json!({"query": "limit zero probe note", "limit": 0})).await;
    assert_eq!(by_limit, 0, "limit=0 returns no hits, the same as top_k=0");
}

/// Exercises `$` sanitization; serialized because non-empty recall tracks background work.
#[tokio::test]
#[serial(background_tasks)]
#[serial_test::serial(config_ledger)]
async fn recall_with_dollar_sign_query_does_not_error() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let ns = Namespace::parse("local").expect("local namespace");
    let token = rt.authorize(ns).expect("authorize local");

    rt.create_note(
        &token,
        "memory",
        None,
        "use $prev.id to chain calls",
        Some(0.7),
        None,
        vec![],
    )
    .await
    .expect("create note");

    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(MemoryPack::new(
        crate::test_support::with_receipt_credentials(rt.clone()),
    ));
    let registry = builder.build().expect("registry");

    let result = registry
        .dispatch(
            "memory.recall",
            serde_json::json!({
                "query": "$prev.id",
                "limit": 10
            }),
        )
        .await;

    assert!(
        result.is_ok(),
        "#388 memory.recall must not hard-fail on a '$'-bearing query, got: {:?}",
        result.err()
    );
}
