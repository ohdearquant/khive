#[tokio::test]
async fn remote_last_pusher_refuses_review_and_merge_then_third_login_succeeds() {
    let f = Fixture::new(true, None).await;
    f.call(&f.reviewer, "git.push", f.push()).await.unwrap();
    assert_eq!(remote_head(&f.remote.bare), Some(f.head.clone()));
    let push = f.last(&f.reviewer).await;
    let n = f.open().await;
    let before = f.remote.writes();
    assert!(f
        .review(&f.reviewer, n)
        .await
        .unwrap_err()
        .to_string()
        .contains("last_pusher"));
    assert_eq!(f.remote.writes(), before);
    let refused = f.last(&f.reviewer).await;
    assert_eq!(refused.disposition, Disposition::NotCommitted);
    assert_eq!(refused.reason.as_deref(), Some("last_pusher"));
    assert_eq!(refused.result["last_pusher"]["push_receipt_id"], push.id);
    assert_eq!(
        refused.result["last_pusher"]["platform_identity"],
        "reviewer"
    );

    // An externally submitted approval by the known pusher is ineligible even
    // if the aggregate decision currently says APPROVED.
    f.remote
        .state
        .lock()
        .unwrap()
        .reviews
        .push(json!({"id":1,"commit_id":f.head,
        "state":"APPROVED","user":{"login":"REVIEWER"}}));
    let refused = f
        .refusal(&f.actor, "git.pr_merge", f.merge(n), "last_pusher")
        .await;
    assert_eq!(refused.result["last_pusher"]["push_receipt_id"], push.id);
    assert_eq!(refused.result["review_decision"]["value"], "APPROVED");
    assert!(!f
        .remote
        .state
        .lock()
        .unwrap()
        .calls
        .iter()
        .any(|call| call["method"] == "PUT"));

    let review = f.review(&f.third, n).await.unwrap();
    assert_eq!(review["last_pusher"]["push_receipt_id"], push.id);
    let result = f.call(&f.actor, "git.pr_merge", f.merge(n)).await.unwrap();
    assert_eq!(f.remote.pr(n)["merged"], true);
    assert_eq!(result["last_pusher"]["push_receipt_id"], push.id);
    assert_eq!(result["review_decision"]["value"], "APPROVED");
    assert_eq!(f.last(&f.actor).await.result, result);
}

#[tokio::test]
async fn remote_unknown_pusher_allows_review_but_platform_decision_gates_merge() {
    let f = Fixture::new(true, None).await;
    let n = f.open().await;
    let review = f.review(&f.reviewer, n).await.unwrap();
    let unknown = json!({"state":"unknown","reason":"no_push_receipt"});
    assert_eq!(review["last_pusher"], unknown);
    assert_eq!(f.last(&f.reviewer).await.result["last_pusher"], unknown);
    assert_eq!(
        f.remote.state.lock().unwrap().reviews[0]["state"],
        "APPROVED"
    );
    for decision in [
        json!("REVIEW_REQUIRED"),
        json!("CHANGES_REQUESTED"),
        Value::Null,
    ] {
        f.remote.state.lock().unwrap().review_decision = decision.clone();
        let refused = f
            .refusal(&f.actor, "git.pr_merge", f.merge(n), "review_decision")
            .await;
        assert_eq!(refused.reason.as_deref(), Some("review_decision"));
        assert_eq!(refused.result["review_decision"]["value"], decision);
        assert_eq!(refused.result["last_pusher"], unknown);
        assert!(!f
            .remote
            .state
            .lock()
            .unwrap()
            .calls
            .iter()
            .any(|call| call["method"] == "PUT"));
    }
    f.remote.state.lock().unwrap().review_decision = json!("APPROVED");
    f.call(&f.actor, "git.pr_merge", f.merge(n)).await.unwrap();
    assert_eq!(f.remote.pr(n)["merged"], true);
}

#[tokio::test]
async fn remote_review_decision_is_bound_to_the_compared_head() {
    let f = Fixture::new(true, None).await;
    let n = f.open().await;
    f.review(&f.reviewer, n).await.unwrap();
    f.remote.state.lock().unwrap().decision_head = Some(f.middle.clone());
    let refused = f
        .refusal(
            &f.actor,
            "git.pr_merge",
            f.merge(n),
            "expected_head_mismatch",
        )
        .await;
    assert_eq!(refused.result["review_decision"]["head_sha"], f.middle);
    assert!(!f
        .remote
        .state
        .lock()
        .unwrap()
        .calls
        .iter()
        .any(|call| call["method"] == "PUT"));
    f.remote.state.lock().unwrap().decision_head = None;
    f.call(&f.actor, "git.pr_merge", f.merge(n)).await.unwrap();
}

#[tokio::test]
async fn remote_push_ledger_uses_newest_acknowledged_matching_receipt() {
    let f = Fixture::new(true, None).await;
    f.call(&f.reviewer, "git.push", f.push()).await.unwrap();
    // Simulate the ref moving away, then a later acknowledged push at the same
    // SHA by a different login. Commit identity cannot distinguish these pushes.
    git(&f.remote.bare, &["update-ref", "refs/heads/work", &f.base]);
    f.call(&f.third, "git.push", f.push()).await.unwrap();
    let acknowledged = f.last(&f.third).await;
    // A newer lost-ACK attempt at the same SHA is not an acknowledged push.
    git(&f.remote.bare, &["update-ref", "refs/heads/work", &f.base]);
    f.remote.state.lock().unwrap().lost_ack = true;
    assert!(f.call(&f.reviewer, "git.push", f.push()).await.is_err());
    assert_eq!(f.last(&f.reviewer).await.disposition, Disposition::Unknown);
    f.remote.state.lock().unwrap().lost_ack = false;
    let n = f.open().await;
    let reviewed = f.review(&f.reviewer, n).await.unwrap();
    assert_eq!(reviewed["last_pusher"]["push_receipt_id"], acknowledged.id);
    assert_eq!(reviewed["last_pusher"]["platform_identity"], "third");
    assert!(f
        .review(&f.third, n)
        .await
        .unwrap_err()
        .to_string()
        .contains("last_pusher"));
    assert_eq!(
        f.last(&f.third).await.result["last_pusher"]["push_receipt_id"],
        acknowledged.id
    );
}

#[tokio::test]
async fn remote_push_ledger_does_not_confuse_repository_ref_or_sha() {
    for mismatch in ["repository", "ref", "sha"] {
        let f = Fixture::new(true, None).await;
        f.call(&f.reviewer, "git.push", f.push()).await.unwrap();
        let n = f.open().await;
        {
            let mut s = f.remote.state.lock().unwrap();
            let pr = s.prs.get_mut(&n).unwrap();
            match mismatch {
                "repository" => pr["head"]["repo"]["full_name"] = json!("fork/project"),
                "ref" => pr["head"]["ref"] = json!("other"),
                _ => pr["head"]["sha"] = json!(f.middle),
            }
        }
        f.policy(&f.reviewer, "git.pr_review.fork", "allow").await;
        let expected = if mismatch == "sha" {
            &f.middle
        } else {
            &f.head
        };
        let review = f
            .call(
                &f.reviewer,
                "git.pr_review",
                json!({"number":n,"verdict":"approve",
            "body":"","expected_head":expected}),
            )
            .await
            .unwrap();
        assert_eq!(
            review["last_pusher"],
            json!({"state":"unknown","reason":"no_push_receipt"})
        );
    }
}
