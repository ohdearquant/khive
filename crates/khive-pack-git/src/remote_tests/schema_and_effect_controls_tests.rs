#[tokio::test]
async fn remote_pack_schema_covers_all_verbs_with_required_nullable_compare() {
    let f = Fixture::new(true, None).await;
    // Derived from the pack's own handler table, never a list typed here: a hand-kept copy passes
    // while it is two verbs behind, which is the failure this test's name promises to catch.
    let verbs: Vec<&str> = crate::vocab::GIT_HANDLERS
        .iter()
        .map(|handler| handler.name)
        .collect();
    assert!(
        verbs.len() >= 15,
        "the handler table is the population under test: {verbs:?}"
    );
    for verb in verbs {
        let help = f
            .registry
            .describe_verb(verb)
            .unwrap_or_else(|_| panic!("{verb} is declared in GIT_HANDLERS but not registered"));
        assert_eq!(help["input_schema"]["type"], "object", "{verb}");
        assert_eq!(
            help["input_schema"]["additionalProperties"], false,
            "{verb}"
        );
    }
    let help = f.registry.describe_verb("git.push").unwrap();
    let schema = &help["input_schema"];
    assert!(schema["required"]
        .as_array()
        .unwrap()
        .contains(&json!("expected_remote")));
    assert_eq!(
        schema["properties"]["expected_remote"]["type"],
        json!(["string", "null"])
    );
    assert_eq!(schema["properties"]["force"]["type"], "boolean");
    assert!(!schema["required"]
        .as_array()
        .unwrap()
        .contains(&json!("force")));
    assert!(help["description"]
        .as_str()
        .unwrap()
        .contains("force_denied"));
    assert!(help["params"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["name"] == "force"
            && p["type"] == "boolean"
            && p["required"] == false
            && p["description"].as_str().unwrap().contains("force_denied")));
    assert!(help["params"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["name"] == "expected_remote"
            && p["type"] == "string|null"
            && p["required"] == true));
}

#[tokio::test]
async fn remote_push_refuses_remote_move_at_effect_control() {
    let f = Fixture::new(true, None).await;
    f.remote.state.lock().unwrap().remote_race = Some(f.middle.clone());
    let result = f.call(&f.actor, "git.push", f.push()).await;
    assert_eq!(
        remote_head(&f.remote.bare),
        Some(f.middle.clone()),
        "candidate={} result={result:?}",
        f.head
    );
    assert!(
        result.is_err(),
        "removed compare permitted a push over the rival ref"
    );
    assert_eq!(
        f.last(&f.actor).await.disposition,
        Disposition::NotCommitted
    );
}

#[tokio::test]
async fn remote_merge_grants_bind_actor_tool_and_expiry() {
    for condition in ["expired", "foreign", "wrong-tool", "valid"] {
        let f = Fixture::new(true, None).await;
        let n = f.open().await;
        f.review(&f.reviewer, n).await.unwrap();
        f.policy(&f.actor, "git.pr_merge", "ask").await;
        let actor = if condition == "foreign" {
            &f.alias
        } else {
            &f.actor
        };
        let tool = if condition == "wrong-tool" {
            "git.pr_open"
        } else {
            "git.pr_merge"
        };
        f.policy(actor, tool, "ask").await;
        let request=f.registry.dispatch("tool.request",json!({"actor":actor,"tool":tool,"scope":"free text, not an authorization constraint"})).await.unwrap();
        let id = request["request_id"].as_str().unwrap();
        let mut grant = json!({"id":id});
        if condition == "expired" {
            grant["expires_in_s"] = json!(0);
        }
        f.registry
            .dispatch_with_identity(
                "tool.grant",
                grant,
                Some(RequestIdentity {
                    namespace: "local".into(),
                    actor_id: Some(format!("{}:operator", f.actor)),
                    ..Default::default()
                }),
            )
            .await
            .unwrap();
        if condition == "valid" {
            f.call(&f.actor, "git.pr_merge", f.merge(n)).await.unwrap();
            let receipt = f.last(&f.actor).await;
            assert_eq!(receipt.policy["source"], "grant");
            assert_eq!(receipt.policy["id"], id);
        } else {
            f.refusal(&f.actor, "git.pr_merge", f.merge(n), "policy_denied")
                .await;
        }
    }
}

#[tokio::test]
async fn remote_push_non_fast_forward_and_unmapped_actor_refuse() {
    let f = Fixture::new(true, None).await;
    let unmapped = format!("{}:unmapped", f.actor);
    f.policy(&unmapped, "git.push", "allow").await;
    let receipt = f
        .refusal(&unmapped, "git.push", f.push(), "actor_unmapped")
        .await;
    // The reason names neither the table it read nor which of the two states it
    // found there (ADR-182 Amendment 10 item 2).
    assert_eq!(
        receipt.result["refusal"],
        json!({"table":"git_write.actors","key":unmapped,"cause":"absent"})
    );
    assert!(f.remote.state.lock().unwrap().calls.is_empty());
    git(&f.remote.bare, &["update-ref", "refs/heads/work", &f.head]);
    git(&f.repo, &["update-ref", "refs/heads/work", &f.base]);
    f.refusal(
        &f.actor,
        "git.push",
        json!({"branch":"work","expected_local":f.base,"expected_remote":f.head}),
        "non_fast_forward",
    )
    .await;
    assert_eq!(f.remote.state.lock().unwrap().push_calls, 0);
}

#[tokio::test]
async fn remote_stale_or_dismissed_approvals_do_not_authorize_merge() {
    let f = Fixture::new(true, None).await;
    let n = f.open().await;
    f.review(&f.reviewer, n).await.unwrap();
    f.remote.state.lock().unwrap().reviews[0]["commit_id"] = json!(f.middle);
    f.refusal(&f.actor, "git.pr_merge", f.merge(n), "missing_review")
        .await;
    f.remote.state.lock().unwrap().reviews[0]["commit_id"] = json!(f.head);
    f.remote
        .state
        .lock()
        .unwrap()
        .reviews
        .push(json!({"id":2,"commit_id":f.head,"state":"DISMISSED","user":{"login":"reviewer"}}));
    f.refusal(&f.actor, "git.pr_merge", f.merge(n), "missing_review")
        .await;
}

#[tokio::test]
async fn remote_merge_refusals_name_opener_and_last_pusher_then_the_approving_login_merges() {
    let f = Fixture::with_merge_refusals(true, None, &["opener", "last_pusher"]).await;
    f.call(&f.reviewer, "git.push", f.push()).await.unwrap();
    let push = f.last(&f.reviewer).await;
    let n = f.open().await;
    f.review(&f.third, n).await.unwrap();
    // The opening actor, read at the platform login.
    let refused = f
        .refusal(&f.actor, "git.pr_merge", f.merge(n), "merge_by_opener")
        .await;
    assert_eq!(refused.result["merge_refusal"]["name"], "merge_by_opener");
    assert_eq!(
        refused.result["merge_refusal"]["evidence"],
        "platform_login"
    );
    // A second actor on the opener's platform account never opened through the pack.
    let refused = f
        .refusal(&f.alias, "git.pr_merge", f.merge(n), "merge_by_opener")
        .await;
    assert_eq!(
        refused.result["merge_refusal"]["evidence"],
        "platform_login"
    );
    // The last pusher of expected_head, cited by its push receipt.
    let refused = f
        .refusal(
            &f.reviewer,
            "git.pr_merge",
            f.merge(n),
            "merge_by_last_pusher",
        )
        .await;
    assert_eq!(
        refused.result["merge_refusal"]["name"],
        "merge_by_last_pusher"
    );
    assert_eq!(refused.result["last_pusher"]["push_receipt_id"], push.id);
    assert!(!f
        .remote
        .state
        .lock()
        .unwrap()
        .calls
        .iter()
        .any(|call| call["method"] == "PUT"));
    // Permit arm: the approving login is neither the opener nor the last pusher.
    let result = f.call(&f.third, "git.pr_merge", f.merge(n)).await.unwrap();
    assert_eq!(f.remote.pr(n)["merged"], true);
    assert_eq!(result["merged_head_sha"], f.head);
    assert_eq!(result["last_pusher"]["push_receipt_id"], push.id);
    assert_eq!(f.last(&f.third).await.result["merge_refusal"], Value::Null);
}
