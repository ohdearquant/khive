fn state_check_run(name: &str, status: &str, conclusion: Value) -> Value {
    json!({"id":7,"name":name,"status":status,"conclusion":conclusion,
        "app":{"slug":"github-actions"},"check_suite":{"id":11},
        "started_at":"2026-11-21T15:00:00Z","completed_at":null})
}

async fn state_receipt_count(f: &Fixture, actor: &str) -> usize {
    receipts::list_owned(&f.rt, "local", actor, None, None, 500, 0)
        .await
        .unwrap()
        .receipts
        .len()
}

fn state_calls(f: &Fixture) -> Vec<Value> {
    f.remote.state.lock().unwrap().calls.clone()
}

#[tokio::test]
async fn remote_pr_state_reports_the_platform_rows_and_writes_nothing() {
    let f = Fixture::new(true, None).await;
    let n = f.open().await;
    f.review(&f.reviewer, n).await.unwrap();
    {
        let mut s = f.remote.state.lock().unwrap();
        // An approval at an older head, from a second account, must read as not at the head.
        s.reviews.push(json!({"id":90,"commit_id":f.middle,"state":"APPROVED","user":{"login":"Third"}}));
        // A later comment by the reviewer does not replace the approval.
        s.reviews.push(json!({"id":91,"commit_id":f.head,"state":"COMMENTED","user":{"login":"reviewer"}}));
        s.check_runs = vec![
            state_check_run("CI / 构建 (macos)", "completed", json!("success")),
            state_check_run("lint", "completed", json!("failure")),
            state_check_run("bench", "in_progress", Value::Null),
        ];
        s.combined = json!({"state":"success","total_count":1,
            "statuses":[{"context":"deploy/preview","state":"success","updated_at":"2026-11-21T15:01:00Z"}]});
        let pr = s.prs.get_mut(&n).unwrap();
        pr["draft"] = json!(false);
        pr["mergeable"] = json!(true);
        pr["mergeable_state"] = json!("blocked");
        pr["base"]["ref"] = json!("main");
        pr["base"]["sha"] = json!(f.base);
        // Before a merge the platform keeps a trial merge commit in this field.
        pr["merge_commit_sha"] = json!("e".repeat(40));
    }
    f.policy(&f.actor, "git.pr_state", "allow").await;
    let writes = f.remote.writes();
    let receipts_before = state_receipt_count(&f, &f.actor).await;
    let calls_before = state_calls(&f).len();
    let refs = git(&f.repo, &["show-ref"]);

    let state = f
        .call(&f.actor, "git.pr_state", json!({"number":n}))
        .await
        .unwrap();

    assert_eq!(state["slug"], SLUG);
    assert_eq!(state["number"], n);
    assert_eq!(state["gate"]["decision"], "allow");
    assert_eq!(state["state"], "open");
    assert_eq!(state["merged"], false);
    assert_eq!(state["draft"], false);
    assert_eq!(state["opened_by"], "author");
    assert_eq!(
        state["head"],
        json!({"ref":"work","sha":f.head,"fork":false})
    );
    assert_eq!(state["base"], json!({"ref":"main","sha":f.base}));
    assert_eq!(state["mergeable"], true);
    assert_eq!(state["mergeable_state"], "blocked");
    assert_eq!(
        state["merged_sha"],
        Value::Null,
        "a trial merge commit is not a merge"
    );
    assert_eq!(state["checks"]["head_sha"], f.head);
    assert_eq!(state["checks"]["total"], 3);
    assert_eq!(state["checks"]["truncated"], false);
    assert_eq!(
        state["checks"]["counts"],
        json!({"failure":1,"in_progress":1,"success":1})
    );
    assert_eq!(state["checks"]["runs"][0]["name"], "CI / 构建 (macos)");
    assert_eq!(state["checks"]["runs"][0]["app"], "github-actions");
    assert_eq!(state["checks"]["runs"][0]["suite_id"], 11);
    assert_eq!(state["checks"]["runs"][2]["status"], "in_progress");
    assert_eq!(state["checks"]["runs"][2]["conclusion"], Value::Null);
    assert_eq!(state["statuses"]["state"], "success");
    assert_eq!(state["statuses"]["total"], 1);
    assert_eq!(state["statuses"]["contexts"][0]["context"], "deploy/preview");
    assert_eq!(
        state["reviews"],
        json!([
            {"login":"reviewer","state":"APPROVED","commit_id":f.head,"at_head":true},
            {"login":"third","state":"APPROVED","commit_id":f.middle,"at_head":false},
        ])
    );
    assert_eq!(state["text_altered"], false);

    // Nothing written: no platform write, no receipt, no ref.
    assert_eq!(f.remote.writes(), writes);
    assert_eq!(state_receipt_count(&f, &f.actor).await, receipts_before);
    assert_eq!(git(&f.repo, &["show-ref"]), refs);
    // Every read went out as a GET under the caller's own credential, and the checks and the
    // statuses were asked for at the head the pull request reported.
    let calls = state_calls(&f)[calls_before..].to_vec();
    assert!(calls.len() >= 5, "{calls:?}");
    for call in &calls {
        assert_eq!(call["method"], "GET", "{call}");
        assert_eq!(
            call["token_hash"],
            blake3::hash(SECRET.as_bytes()).to_hex().to_string()
        );
    }
    let paths: Vec<&str> = calls
        .iter()
        .map(|call| call["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths[0], format!("repos/{SLUG}"));
    assert_eq!(paths[1], format!("repos/{SLUG}/pulls/{n}"));
    assert!(paths.contains(
        &format!(
            "repos/{SLUG}/commits/{}/check-runs?per_page=100&page=1",
            f.head
        )
        .as_str()
    ));
    assert!(paths
        .contains(&format!("repos/{SLUG}/commits/{}/status?per_page=100", f.head).as_str()));
}

#[tokio::test]
async fn remote_pr_state_with_nothing_posted_says_so_in_counts_not_in_a_verdict() {
    let f = Fixture::new(true, None).await;
    let n = f.open().await;
    f.policy(&f.actor, "git.pr_state", "allow").await;
    let state = f
        .call(&f.actor, "git.pr_state", json!({"number":n}))
        .await
        .unwrap();
    assert_eq!(state["checks"]["total"], 0);
    assert_eq!(state["checks"]["runs"], json!([]));
    assert_eq!(state["checks"]["counts"], json!({}));
    // The platform's combined state reads "pending" when no status exists; total says none.
    assert_eq!(state["statuses"]["state"], "pending");
    assert_eq!(state["statuses"]["total"], 0);
    assert_eq!(state["reviews"], json!([]));
    assert_eq!(state["mergeable"], Value::Null);
    assert_eq!(state["mergeable_state"], Value::Null);
}

#[tokio::test]
async fn remote_pr_state_refuses_before_any_platform_read() {
    let f = Fixture::new(true, None).await;
    let n = f.open().await;
    let calls = state_calls(&f).len();
    let reads = std::fs::read_to_string(f.dir.path().join("reads")).unwrap();
    let untouched = |f: &Fixture, arm: &str| {
        assert_eq!(state_calls(f).len(), calls, "{arm} reached the platform");
        assert_eq!(
            std::fs::read_to_string(f.dir.path().join("reads")).unwrap(),
            reads,
            "{arm} resolved a credential"
        );
    };

    // No policy row for this verb yet.
    let error = f
        .call(&f.actor, "git.pr_state", json!({"number":n}))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("policy_denied"), "{error}");
    untouched(&f, "policy");

    f.policy(&f.actor, "git.pr_state", "allow").await;
    for (arm, params, reason) in [
        ("zero", json!({"number":0}), "invalid_params"),
        ("text", json!({"number":"1"}), "invalid_params"),
        ("absent", json!({}), "invalid_params"),
        (
            "unknown",
            json!({"number":n,"expected_head":f.head}),
            "unknown field `expected_head`",
        ),
    ] {
        let error = f
            .call(&f.actor, "git.pr_state", params)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains(reason), "{arm}: {error}");
        untouched(&f, arm);
    }

    // A caller with an allow decision and no actor row has no credential to read with.
    let stranger = format!("{}:stranger", f.actor);
    f.policy(&stranger, "git.pr_state", "allow").await;
    let error = f
        .call(&stranger, "git.pr_state", json!({"number":n}))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("actor_unmapped"), "{error}");
    untouched(&f, "stranger");
    assert_eq!(state_receipt_count(&f, &stranger).await, 0);
}

#[tokio::test]
async fn remote_pr_state_refuses_an_unlisted_repository_before_policy() {
    let f = Fixture::new(false, None).await;
    f.policy(&f.actor, "git.pr_state", "allow").await;
    let checks = crate::local_handlers::policy_check_count(&f.actor);
    let error = f
        .call(&f.actor, "git.pr_state", json!({"number":1}))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("not_configured"), "{error}");
    assert_eq!(crate::local_handlers::policy_check_count(&f.actor), checks);
    assert!(state_calls(&f).is_empty());
    assert!(!f.dir.path().join("reads").exists());
}

#[tokio::test]
async fn remote_pr_state_stops_at_a_repository_that_answers_under_another_identity() {
    let f = Fixture::new(true, None).await;
    let n = f.open().await;
    f.policy(&f.actor, "git.pr_state", "allow").await;
    for field in ["slug", "visibility"] {
        f.remote.state.lock().unwrap().mismatch = Some(field);
        let before = state_calls(&f).len();
        let error = f
            .call(&f.actor, "git.pr_state", json!({"number":n}))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("repository_identity_mismatch"), "{error}");
        let calls = state_calls(&f)[before..].to_vec();
        assert_eq!(calls.len(), 1, "{field}: {calls:?}");
        assert_eq!(calls[0]["path"], format!("repos/{SLUG}"));
    }
    f.remote.state.lock().unwrap().mismatch = None;
    // Control: the same call answers once the identity matches, and a number the platform
    // does not know is the platform's refusal, not an empty state.
    f.call(&f.actor, "git.pr_state", json!({"number":n}))
        .await
        .unwrap();
    let error = f
        .call(&f.actor, "git.pr_state", json!({"number":99}))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("remote_refused"), "{error}");
    f.remote.state.lock().unwrap().api_failure = true;
    let error = f
        .call(&f.actor, "git.pr_state", json!({"number":n}))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("remote_unavailable"), "{error}");
}

#[tokio::test]
async fn remote_pr_state_removes_hidden_characters_and_says_it_did() {
    let f = Fixture::new(true, None).await;
    let n = f.open().await;
    f.policy(&f.actor, "git.pr_state", "allow").await;
    {
        let mut s = f.remote.state.lock().unwrap();
        s.check_runs = vec![state_check_run(
            "build\u{202E}gnp.exe\u{001B}[2J\u{200B}",
            "completed",
            json!("success"),
        )];
        s.prs.get_mut(&n).unwrap()["head"]["ref"] = json!(format!("work\u{2066}{}", "x".repeat(400)));
    }
    let state = f
        .call(&f.actor, "git.pr_state", json!({"number":n}))
        .await
        .unwrap();
    assert_eq!(state["checks"]["runs"][0]["name"], "buildgnp.exe[2J");
    let branch = state["head"]["ref"].as_str().unwrap();
    assert_eq!(branch.chars().count(), 255);
    assert!(branch.starts_with("workxxx"));
    assert_eq!(state["text_altered"], true);
    // Control: a name with nothing hidden in it leaves the flag down.
    f.remote.state.lock().unwrap().check_runs = vec![state_check_run(
        "CI / 构建 — é",
        "completed",
        json!("success"),
    )];
    f.remote.state.lock().unwrap().prs.get_mut(&n).unwrap()["head"]["ref"] = json!("work");
    let state = f
        .call(&f.actor, "git.pr_state", json!({"number":n}))
        .await
        .unwrap();
    assert_eq!(state["checks"]["runs"][0]["name"], "CI / 构建 — é");
    assert_eq!(state["text_altered"], false);
}

#[tokio::test]
async fn remote_pr_state_pages_check_runs_and_names_what_it_did_not_read() {
    let f = Fixture::new(true, None).await;
    let n = f.open().await;
    f.policy(&f.actor, "git.pr_state", "allow").await;
    let many = |count: usize| -> Vec<Value> {
        (0..count)
            .map(|i| state_check_run(&format!("job-{i}"), "completed", json!("success")))
            .collect()
    };
    f.remote.state.lock().unwrap().check_runs = many(250);
    let before = state_calls(&f).len();
    let state = f
        .call(&f.actor, "git.pr_state", json!({"number":n}))
        .await
        .unwrap();
    assert_eq!(state["checks"]["total"], 250);
    assert_eq!(state["checks"]["runs"].as_array().unwrap().len(), 250);
    assert_eq!(state["checks"]["counts"], json!({"success":250}));
    assert_eq!(state["checks"]["truncated"], false);
    assert_eq!(state["checks"]["runs"][249]["name"], "job-249");
    let pages = state_calls(&f)[before..]
        .iter()
        .filter(|call| call["path"].as_str().unwrap().contains("/check-runs?"))
        .count();
    assert_eq!(pages, 3);

    f.remote.state.lock().unwrap().check_runs = many(620);
    let state = f
        .call(&f.actor, "git.pr_state", json!({"number":n}))
        .await
        .unwrap();
    assert_eq!(state["checks"]["total"], 620);
    assert_eq!(state["checks"]["runs"].as_array().unwrap().len(), 500);
    assert_eq!(state["checks"]["truncated"], true);
}

#[tokio::test]
async fn remote_pr_state_reports_a_merge_and_refuses_an_unbounded_review_list() {
    let f = Fixture::new(true, None).await;
    let n = f.open().await;
    f.review(&f.reviewer, n).await.unwrap();
    f.call(&f.actor, "git.pr_merge", f.merge(n)).await.unwrap();
    f.policy(&f.actor, "git.pr_state", "allow").await;
    let state = f
        .call(&f.actor, "git.pr_state", json!({"number":n}))
        .await
        .unwrap();
    assert_eq!(state["state"], "closed");
    assert_eq!(state["merged"], true);
    assert_eq!(state["merged_sha"], "d".repeat(40));

    // The fixture answers every review page with the whole list, so one hundred rows never end.
    let row = json!({"id":1,"commit_id":f.head,"state":"APPROVED","user":{"login":"reviewer"}});
    f.remote.state.lock().unwrap().reviews = vec![row; 100];
    let error = f
        .call(&f.actor, "git.pr_state", json!({"number":n}))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("review_evidence_limit"), "{error}");
}
