// git.pr_edit and git.pr_close (ADR-182 Amendment 17).

fn edit(f: &Fixture, n: u64, fields: Value) -> Value {
    let mut params = fields;
    params["number"] = json!(n);
    params["expected_head"] = json!(f.head);
    params
}

fn close(f: &Fixture, n: u64) -> Value {
    json!({"number":n, "expected_head":f.head})
}

fn last_patch(f: &Fixture) -> Value {
    f.remote
        .state
        .lock()
        .unwrap()
        .calls
        .iter()
        .rev()
        .find(|call| call["method"] == "PATCH")
        .cloned()
        .expect("a PATCH call")
}

#[tokio::test]
async fn the_opener_edits_title_and_body_with_one_bound_write() {
    let f = Fixture::new(true, None).await;
    let n = f.open().await;
    let before = f.remote.writes();
    let result = f
        .call(
            &f.actor,
            "git.pr_edit",
            edit(&f, n, json!({"title":"Better title","body":"Better body"})),
        )
        .await
        .unwrap();
    assert_eq!(result["number"], n);
    assert_eq!(result["head_sha"], f.head);
    assert_eq!(result["state"], "open");
    assert_eq!(result["edited"], json!(["body", "title"]));
    assert_eq!(result["authority"], json!({"source":"platform_login"}));
    assert_eq!(f.remote.writes(), before + 1);
    let patch = last_patch(&f);
    assert_eq!(patch["path"], format!("repos/{SLUG}/pulls/{n}"));
    assert_eq!(
        patch["body"],
        json!({"title":"Better title","body":"Better body"})
    );
    let pr = f.remote.pr(n);
    assert_eq!(pr["title"], "Better title");
    assert_eq!(pr["body"], "Better body");
    let receipt = f.last(&f.actor).await;
    assert_eq!(receipt.verb, "git.pr_edit");
    assert_eq!(receipt.disposition, Disposition::Committed);
    assert_eq!(receipt.inputs["title"], "Better title");
}

#[tokio::test]
async fn an_edit_sends_only_the_fields_named_and_an_empty_body_reads_back() {
    let f = Fixture::new(true, None).await;
    let n = f.open().await;
    f.call(&f.actor, "git.pr_edit", edit(&f, n, json!({"body":""})))
        .await
        .unwrap();
    assert_eq!(last_patch(&f)["body"], json!({"body":""}));
    // The platform answers an empty body as null; the title is untouched.
    assert_eq!(f.remote.pr(n)["body"], Value::Null);
    assert_eq!(f.remote.pr(n)["title"], "Fixture");
    let result = f
        .call(&f.actor, "git.pr_edit", edit(&f, n, json!({"title":"Only title"})))
        .await
        .unwrap();
    assert_eq!(result["edited"], json!(["title"]));
    assert_eq!(last_patch(&f)["body"], json!({"title":"Only title"}));
}

#[tokio::test]
async fn an_edit_naming_nothing_or_an_empty_title_refuses_before_any_platform_call() {
    let f = Fixture::new(true, None).await;
    let n = f.open().await;
    let calls = f.remote.state.lock().unwrap().calls.len();
    for fields in [
        json!({}),
        json!({"title":""}),
        json!({"title":7}),
        json!({"title":" Padded"}),
        json!({"title":"Padded "}),
        json!({"title":"Two\nlines"}),
    ] {
        let error = f
            .call(&f.actor, "git.pr_edit", edit(&f, n, fields.clone()))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("invalid_params"), "{fields}: {error}");
    }
    assert_eq!(f.remote.state.lock().unwrap().calls.len(), calls);
}

#[tokio::test]
async fn a_caller_who_did_not_open_the_pull_request_needs_the_others_policy() {
    let f = Fixture::new(true, None).await;
    let n = f.open().await;
    for (verb, params) in [
        ("git.pr_edit", edit(&f, n, json!({"title":"Not mine"}))),
        ("git.pr_close", close(&f, n)),
    ] {
        let receipt = f.refusal(&f.reviewer, verb, params.clone(), "not_opener").await;
        assert_eq!(receipt.result["authority"]["source"], "policy");
        assert_ne!(receipt.result["authority"]["policy"]["decision"], "allow");
        let id = f.policy(&f.reviewer, &format!("{verb}.others"), "allow").await;
        let result = f.call(&f.reviewer, verb, params).await.unwrap();
        assert_eq!(result["authority"]["source"], "policy");
        assert_eq!(result["authority"]["policy"]["decision"], "allow");
        assert_eq!(result["authority"]["policy"]["id"], id);
    }
    assert_eq!(f.remote.pr(n)["title"], "Not mine");
    assert_eq!(f.remote.pr(n)["state"], "closed");
}

#[tokio::test]
async fn a_pr_open_receipt_names_the_opener_when_the_platform_login_differs() {
    let f = Fixture::new(true, None).await;
    // The platform reports another account as the author; this actor's own receipt
    // still shows it opened the pull request.
    f.remote.state.lock().unwrap().author = "third".into();
    let n = f.open().await;
    let result = f
        .call(&f.actor, "git.pr_edit", edit(&f, n, json!({"title":"Mine"})))
        .await
        .unwrap();
    assert_eq!(result["authority"], json!({"source":"pr_open_receipt"}));
    // Control: the account the platform names did not open it here, and holds no policy.
    f.refusal(&f.reviewer, "git.pr_edit", edit(&f, n, json!({"title":"x"})), "not_opener")
        .await;
}

#[tokio::test]
async fn close_closes_an_open_pull_request_once_and_an_edit_still_reaches_it() {
    let f = Fixture::new(true, None).await;
    let n = f.open().await;
    let result = f.call(&f.actor, "git.pr_close", close(&f, n)).await.unwrap();
    assert_eq!(result["state"], "closed");
    assert_eq!(result["head_sha"], f.head);
    assert_eq!(last_patch(&f)["body"], json!({"state":"closed"}));
    assert_eq!(f.remote.pr(n)["merged"], false);
    f.refusal(&f.actor, "git.pr_close", close(&f, n), "pull_request_closed")
        .await;
    let result = f
        .call(&f.actor, "git.pr_edit", edit(&f, n, json!({"body":"Closed: superseded"})))
        .await
        .unwrap();
    assert_eq!(result["state"], "closed");
    assert_eq!(f.remote.pr(n)["body"], "Closed: superseded");
}

#[tokio::test]
async fn a_merged_pull_request_refuses_close() {
    let f = Fixture::new(true, None).await;
    let n = f.open().await;
    {
        let mut s = f.remote.state.lock().unwrap();
        let pr = s.prs.get_mut(&n).unwrap();
        pr["merged"] = json!(true);
        pr["state"] = json!("closed");
    }
    f.refusal(&f.actor, "git.pr_close", close(&f, n), "pull_request_closed")
        .await;
}

#[tokio::test]
async fn a_stale_expected_head_refuses_both_verbs_before_any_write() {
    let f = Fixture::new(true, None).await;
    let n = f.open().await;
    for (verb, mut params) in [
        ("git.pr_edit", edit(&f, n, json!({"title":"Stale"}))),
        ("git.pr_close", close(&f, n)),
    ] {
        params["expected_head"] = json!(f.base);
        f.refusal(&f.actor, verb, params, "expected_head_mismatch")
            .await;
    }
    assert_eq!(f.remote.pr(n)["state"], "open");
    assert_eq!(f.remote.pr(n)["title"], "Fixture");
}

#[tokio::test]
async fn a_head_that_moves_during_the_write_leaves_the_receipt_unknown() {
    let f = Fixture::new(true, None).await;
    let n = f.open().await;
    f.remote.state.lock().unwrap().merge_race = Some(f.middle.clone());
    assert!(f
        .call(&f.actor, "git.pr_edit", edit(&f, n, json!({"title":"Raced"})))
        .await
        .is_err());
    let receipt = f.last(&f.actor).await;
    assert_eq!(receipt.disposition, Disposition::Unknown);
    assert_eq!(receipt.reason.as_deref(), Some("changed_head_mismatch"));
    assert_eq!(receipt.result["head_sha"], f.middle);
}

#[tokio::test]
async fn a_lost_reply_to_close_leaves_the_receipt_unknown() {
    let f = Fixture::new(true, None).await;
    let n = f.open().await;
    f.remote.state.lock().unwrap().lost_ack = true;
    assert!(f.call(&f.actor, "git.pr_close", close(&f, n)).await.is_err());
    let receipt = f.last(&f.actor).await;
    assert_eq!(receipt.disposition, Disposition::Unknown);
    assert_eq!(f.remote.pr(n)["state"], "closed");
}

#[tokio::test]
async fn a_second_fixture_beside_a_live_one_fails_at_once() {
    // Each fixture holds the environment lock until it drops, so a second one built beside it
    // would wait for its own test forever; the fixture refuses that at once instead.
    let f = Fixture::new(true, None).await;
    let second = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::spawn(async {
            Fixture::new(true, None).await;
        }),
    )
    .await;
    drop(f);
    let joined = second.expect("a second fixture must refuse at once, not wait on the lock");
    assert!(joined.unwrap_err().is_panic());
}

#[tokio::test]
async fn a_reply_that_does_not_show_the_edit_leaves_the_receipt_unknown() {
    // The platform accepts each write and answers with a record that does not show it: the
    // change may or may not have landed, so the receipt cannot say committed.
    for (verb, fields, reply) in [
        ("git.pr_edit", json!({"title":"Wanted"}), json!({"title":"Other"})),
        ("git.pr_edit", json!({"body":"Wanted"}), json!({"body":7})),
        ("git.pr_close", json!({}), json!({"state":"open"})),
        ("git.pr_close", json!({}), json!({"merged":true})),
        ("git.pr_close", json!({}), json!({"number":99})),
    ] {
        let f = Fixture::new(true, None).await;
        let n = f.open().await;
        let mut answer = f.remote.pr(n);
        if verb == "git.pr_close" {
            answer["state"] = json!("closed");
        }
        for (field, value) in reply.as_object().unwrap() {
            answer[field] = value.clone();
        }
        f.remote.state.lock().unwrap().patch_reply = Some(answer);
        let params = if verb == "git.pr_edit" {
            edit(&f, n, fields)
        } else {
            close(&f, n)
        };
        assert!(f.call(&f.actor, verb, params).await.is_err(), "{verb} {reply}");
        let receipt = f.last(&f.actor).await;
        assert_eq!(receipt.disposition, Disposition::Unknown, "{verb} {reply}");
        assert_eq!(receipt.reason.as_deref(), Some("remote_response"), "{verb} {reply}");
    }
}
