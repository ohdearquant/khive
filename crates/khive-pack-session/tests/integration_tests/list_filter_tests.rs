// ── session.list tests ────────────────────────────────────────────────────────

#[tokio::test]
async fn list_returns_all_sessions_newest_first_without_content() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("list.db"));
    let registry = build_registry(rt);

    store_session(&registry, "first").await;
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    store_session(&registry, "second").await;

    let result = registry
        .dispatch("session.list", json!({}))
        .await
        .expect("list ok");
    let sessions = result["sessions"].as_array().expect("sessions array");
    assert_eq!(sessions.len(), 2);
    assert_eq!(result["count"], 2);
    assert!(
        sessions[0]["created_at"].as_str().unwrap() >= sessions[1]["created_at"].as_str().unwrap(),
        "list must be newest-first"
    );
    assert!(
        sessions[0].get("content").is_none(),
        "summaries must not include content"
    );
}

#[tokio::test]
async fn list_filter_by_provider() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("list_provider.db"));
    let registry = build_registry(rt);

    registry
        .dispatch(
            "session.store",
            json!({"content": "alpha session", "provider": "codex"}),
        )
        .await
        .expect("store alpha");
    registry
        .dispatch(
            "session.store",
            json!({"content": "beta session", "provider": "claude_code"}),
        )
        .await
        .expect("store beta");

    let codex_only = registry
        .dispatch("session.list", json!({"provider": "codex"}))
        .await
        .expect("list codex");
    let arr = codex_only["sessions"].as_array().expect("array");
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["provider"], "codex");
}

#[tokio::test]
async fn list_filter_by_unknown_provider_returns_empty() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("list_unknown_provider.db"));
    let registry = build_registry(rt);

    registry
        .dispatch(
            "session.store",
            json!({"content": "alpha session", "provider": "codex"}),
        )
        .await
        .expect("store alpha");

    let result = registry
        .dispatch("session.list", json!({"provider": "nonexistent_provider"}))
        .await
        .expect("list with unknown provider filter must still return ok, not error");
    let arr = result["sessions"].as_array().expect("sessions array");
    assert_eq!(
        arr.len(),
        0,
        "unknown provider filter must yield no matches"
    );
    assert_eq!(result["count"], 0);
}

#[tokio::test]
async fn list_filters_by_agent_id_in_storage() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("list_agent_id.db"));
    let registry = build_registry(rt);

    for (content, agent_id) in [("alpha", "lambda:alpha"), ("beta", "lambda:beta")] {
        registry
            .dispatch(
                "create",
                json!({
                    "kind": "session",
                    "content": content,
                    "properties": {"agent_id": agent_id}
                }),
            )
            .await
            .expect("create attributed session note");
    }

    let result = registry
        .dispatch("session.list", json!({"agent_id": "lambda:alpha"}))
        .await
        .expect("list by agent_id");
    let sessions = result["sessions"].as_array().expect("sessions array");
    assert_eq!(sessions.len(), 1);
    assert_eq!(result["total"], 1);
    assert_eq!(sessions[0]["id"].as_str().map(str::len), Some(36));
}

#[tokio::test]
async fn list_filters_by_inclusive_since_bound_in_storage() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("list_since.db"));
    let registry = build_registry(rt);

    store_session(&registry, "before").await;
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let after = store_session(&registry, "after").await;
    let after_id = after["session"]["id"].as_str().expect("after id");
    let since = after["session"]["created_at"].as_str().expect("created_at");

    let result = registry
        .dispatch("session.list", json!({"since": since}))
        .await
        .expect("list since");
    let sessions = result["sessions"].as_array().expect("sessions array");
    assert_eq!(sessions.len(), 1);
    assert_eq!(result["total"], 1);
    assert_eq!(sessions[0]["id"], after_id);
}

#[tokio::test]
async fn list_limit_respected() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("list_limit.db"));
    let registry = build_registry(rt);

    for i in 0..5 {
        store_session(&registry, &format!("session {i}")).await;
    }

    let limited = registry
        .dispatch("session.list", json!({"limit": 3}))
        .await
        .expect("list limited");
    assert_eq!(limited["sessions"].as_array().expect("array").len(), 3);
    assert_eq!(limited["limit"], 3);
}

#[tokio::test]
async fn list_rejects_out_of_range_limit() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("list_bad_limit.db"));
    let registry = build_registry(rt);

    let zero = registry.dispatch("session.list", json!({"limit": 0})).await;
    assert!(zero.is_err(), "limit=0 must be rejected");
    let too_big = registry
        .dispatch("session.list", json!({"limit": 201}))
        .await;
    assert!(too_big.is_err(), "limit=201 must be rejected");
}

#[tokio::test]
async fn list_offset_pagination() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("list_offset.db"));
    let registry = build_registry(rt);

    for i in 0..5 {
        store_session(&registry, &format!("session {i}")).await;
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    let all = registry
        .dispatch("session.list", json!({"limit": 5}))
        .await
        .expect("full list ok");
    let all_arr = all["sessions"].as_array().expect("all array");
    assert_eq!(all_arr.len(), 5, "expected 5 sessions");

    let paged = registry
        .dispatch("session.list", json!({"offset": 2, "limit": 2}))
        .await
        .expect("paged list ok");
    let paged_arr = paged["sessions"].as_array().expect("paged array");
    assert_eq!(paged_arr.len(), 2, "expected exactly 2 items");
    assert_eq!(paged_arr[0]["id"], all_arr[2]["id"]);
    assert_eq!(paged_arr[1]["id"], all_arr[3]["id"]);
}
