#[tokio::test]
async fn remote_pr_identity_mismatch_has_no_effect_and_open_binds_head() {
    let f = Fixture::new(true, None).await;
    let p = json!({"head":"work","base":"main","title":"Fixture","body":"","expected_head":f.head});
    for field in ["slug", "visibility"] {
        f.remote.state.lock().unwrap().mismatch = Some(field);
        f.refusal(
            &f.actor,
            "git.pr_open",
            p.clone(),
            "repository_identity_mismatch",
        )
        .await;
    }
    f.remote.state.lock().unwrap().mismatch = None;
    let result = f.call(&f.actor, "git.pr_open", p).await.unwrap();
    assert_eq!(result["head_sha"], f.head);
    assert_eq!(result["url"], f.remote.pr(1)["html_url"]);
}

#[tokio::test]
async fn remote_pr_self_actor_and_same_account_refuse_then_second_account_approves() {
    let f = Fixture::new(true, None).await;
    let n = f.open().await;
    for actor in [&f.actor, &f.alias] {
        let before = f.remote.writes();
        let review = f.review(actor, n).await;
        assert_eq!(
            f.remote.writes(),
            before,
            "unexpected platform review: {}",
            json!(f.remote.state.lock().unwrap().reviews)
        );
        assert!(review.unwrap_err().to_string().contains("self_approval"));
        assert_eq!(f.last(actor).await.disposition, Disposition::NotCommitted);
    }
    let result = f.review(&f.reviewer, n).await.unwrap();
    assert_eq!(result["head_sha"], f.head);
    assert_eq!(result["state"], "approved");
    let s = f.remote.state.lock().unwrap();
    assert_eq!(s.reviews[0]["commit_id"], f.head);
    assert_eq!(s.reviews[0]["user"]["login"], "reviewer");
}

#[tokio::test]
async fn remote_review_and_merge_head_races_preserve_rival() {
    for seam in ["review", "before-merge", "during-merge"] {
        let f = Fixture::new(true, None).await;
        let n = f.open().await;
        if seam != "review" {
            f.review(&f.reviewer, n).await.unwrap();
        }
        if seam == "during-merge" {
            f.remote.state.lock().unwrap().merge_race = Some(f.middle.clone());
        } else {
            f.remote.state.lock().unwrap().prs.get_mut(&n).unwrap()["head"]["sha"] =
                json!(f.middle);
        }
        let result = if seam == "review" {
            f.review(&f.reviewer, n).await
        } else {
            f.call(&f.actor, "git.pr_merge", f.merge(n)).await
        };
        assert!(result.is_err());
        assert_eq!(f.remote.pr(n)["head"]["sha"], f.middle);
        assert_eq!(f.remote.pr(n)["merged"], false);
    }
}

#[tokio::test]
async fn remote_merge_policy_and_second_account_review_are_both_required() {
    let f = Fixture::new(true, None).await;
    let n = f.open().await;
    f.refusal(&f.actor, "git.pr_merge", f.merge(n), "missing_review")
        .await;
    f.review(&f.reviewer, n).await.unwrap();
    for decision in ["ask", "deny"] {
        f.policy(&f.actor, "git.pr_merge", decision).await;
        f.refusal(&f.actor, "git.pr_merge", f.merge(n), "policy_denied")
            .await;
    }
    let id = f.policy(&f.actor, "git.pr_merge", "allow").await;
    let result = f.call(&f.actor, "git.pr_merge", f.merge(n)).await.unwrap();
    assert_eq!(result["merged_head_sha"], f.head);
    assert_eq!(result["merged_sha"], f.remote.pr(n)["merge_commit_sha"]);
    assert_eq!(f.last(&f.actor).await.policy["id"], id);
    f.refusal(&f.actor, "git.pr_merge", f.merge(n), "pull_request_closed")
        .await;
}

#[tokio::test]
async fn remote_fork_approval_and_merge_require_named_rows() {
    let f = Fixture::new(true, None).await;
    f.remote.state.lock().unwrap().fork = true;
    let n = f.open().await;
    f.policy(&f.actor, "git.pr_merge.admin", "allow").await;
    for decision in ["ask", "deny"] {
        f.policy(&f.reviewer, "git.pr_review.fork", decision).await;
        assert!(f
            .review(&f.reviewer, n)
            .await
            .unwrap_err()
            .to_string()
            .contains("fork_policy_denied"));
    }
    let id = f.policy(&f.reviewer, "git.pr_review.fork", "allow").await;
    f.review(&f.reviewer, n).await.unwrap();
    assert_eq!(f.last(&f.reviewer).await.fork_policy["id"], id);
    for decision in ["ask", "deny"] {
        f.policy(&f.actor, "git.pr_merge.fork", decision).await;
        f.refusal(&f.actor, "git.pr_merge", f.merge(n), "fork_policy_denied")
            .await;
    }
    let id = f.policy(&f.actor, "git.pr_merge.fork", "allow").await;
    f.call(&f.actor, "git.pr_merge", f.merge(n)).await.unwrap();
    assert_eq!(f.last(&f.actor).await.fork_policy["id"], id);
    assert!(!json!(f.remote.state.lock().unwrap().calls)
        .to_string()
        .contains("admin"));
}

#[tokio::test]
async fn remote_rotated_credential_is_resolved_once_and_never_stored() {
    for error in [false, true] {
        let f = Fixture::new(true, None).await;
        let rotated = format!("{SECRET}-rotated");
        std::fs::write(f.dir.path().join("author-ref"), &rotated).unwrap();
        f.remote.state.lock().unwrap().api_failure = error;
        let reply = f.call(&f.actor, "git.push", f.push()).await;
        if error {
            assert!(reply.is_err());
        } else {
            assert!(reply.is_ok());
        }
        assert_eq!(
            std::fs::read_to_string(f.dir.path().join("reads")).unwrap(),
            "author-ref\n"
        );
        let mut reader = f.rt.sql().reader().await.unwrap();
        let rows = reader
            .query_all(SqlStatement {
                sql: "SELECT * FROM git_receipts".into(),
                params: vec![],
                label: None,
            })
            .await
            .unwrap();
        let receipt = f.last(&f.actor).await;
        let calls = f.remote.state.lock().unwrap().calls.clone();
        let raw = format!("{reply:?} {rows:?} {} {calls:?}", receipt.to_value());
        assert!(!raw.contains(SECRET));
        assert!(!raw.contains("GH_TOKEN="));
        assert!(!raw.contains("Authorization: Bearer "));
        assert_eq!(
            calls[0]["token_hash"],
            blake3::hash(rotated.as_bytes()).to_hex().to_string()
        );
    }
}
