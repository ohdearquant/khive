#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn receipts_page_by_actor_session_and_never_echo_resolved_secrets() {
    if crate::test_process::run_in_child() {
        return;
    }

    let f = Fixture::new(true, true).await;
    f.policy("git.commit", "allow").await;
    let manifest = f.tree(&[("a.txt", b"new\n", 644)]).await;
    let result = f.call("git.commit", f.commit_params(&manifest)).await;
    f.success_receipt(&result).await;
    std::fs::write(&f.secret, b"rotated-synthetic-token\n").expect("rotate fixture secret");
    let mut params = f.commit_params(&manifest);
    params["expected_head"] = result["sha"].clone();
    let second = f.call("git.commit", params).await;
    assert_eq!(f.resolver_count(), 2);
    f.call(
        "tool.policy",
        json!({"actor":"actor:other","tool":"git.checkout","decision":"allow"}),
    )
    .await;
    let foreign = f
        .registry
        .dispatch_with_identity(
            "git.checkout",
            json!({"repo":f.repo,"ref":"HEAD","session_id":"session-a"}),
            Some(RequestIdentity {
                namespace: "local".into(),
                actor_id: Some("actor:other".into()),
                ..Default::default()
            }),
        )
        .await
        .expect("foreign read fixture");
    let first_page = f
        .call(
            "git.receipts",
            json!({"repo":f.repo,"session_id":"session-a","limit":1,"offset":0}),
        )
        .await;
    assert_eq!(first_page["receipts"].as_array().unwrap().len(), 1);
    assert_eq!(first_page["receipts"][0]["id"], result["receipt_id"]);
    assert_eq!(first_page["next_offset"], 1);
    let last_page = f
        .call(
            "git.receipts",
            json!({"repo":f.repo,"session_id":"session-a","limit":1,"offset":1}),
        )
        .await;
    assert_eq!(last_page["receipts"][0]["id"], second["receipt_id"]);
    assert!(last_page["next_offset"].is_null());
    let all = f
        .call("git.receipts", json!({"limit":500,"offset":0}))
        .await;
    assert!(!all
        .to_string()
        .contains(foreign["receipt_id"].as_str().unwrap()));
    for secret in [TOKEN, "rotated-synthetic-token"] {
        assert!(!all.to_string().contains(secret));
        assert!(!result.to_string().contains(secret));
        assert!(!second.to_string().contains(secret));
    }
    let mut reader = f.rt.sql().reader().await.expect("SQL reader");
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT * FROM git_receipts".into(),
            params: vec![],
            label: Some("git_test_credential_census".into()),
        })
        .await
        .expect("actual durable receipt rows");
    drop(reader);
    let stored = serde_json::to_string(&rows).expect("raw row serialization");
    assert!(!stored.contains(TOKEN));
    assert!(!stored.contains("rotated-synthetic-token"));
    assert!(f
        .err(
            "git.receipts",
            json!({"actor":"actor:other","limit":1,"offset":0})
        )
        .await
        .contains("foreign_actor"));
    assert_eq!(
        f.call("git.receipts", json!({"limit":500,"offset":0}))
            .await,
        all,
        "audit reads do not recursively receipt themselves"
    );
    std::fs::write(&f.secret, b"").expect("rotate to invalid credential");
    let mut params = f.commit_params(&manifest);
    params["expected_head"] = second["sha"].clone();
    let error = f.err("git.commit", params).await;
    assert_eq!(f.refusal_receipt(&error).await["reason"], "actor_unmapped");
    assert_eq!(f.resolver_count(), 3, "each mutation rereads the resolver");
}
