fn listed_pull_request(number: u64, title: &str) -> Value {
    json!({
        "number": number,
        "title": title,
        "state": "open",
        "draft": false,
        "user": {"login": "pull-author"},
        "head": {"ref": format!("feature-{number}"), "sha": "a".repeat(40)},
        "base": {"ref": "main"},
        "created_at": "2026-11-20T10:00:00Z",
        "updated_at": "2026-11-21T11:00:00Z"
    })
}

fn listed_issue(number: u64, title: &str, author: Option<&str>, is_pull_request: bool) -> Value {
    let mut issue = json!({
        "number": number,
        "title": title,
        "state": "open",
        "user": author.map(|login| json!({"login": login})).unwrap_or(Value::Null),
        "labels": [{"name": "bug"}],
        "comments": 3,
        "created_at": "2026-11-20T10:00:00Z",
        "updated_at": "2026-11-21T11:00:00Z"
    });
    if is_pull_request {
        issue["pull_request"] =
            json!({"url":"https://api.github.com/repos/fixture/project/pulls/8"});
    }
    issue
}

fn list_path(resource: &str, state: &str, per_page: u64, page: u64) -> String {
    format!(
        "repos/{SLUG}/{resource}?state={state}&sort=created&direction=desc&per_page={per_page}&page={page}"
    )
}

#[tokio::test]
async fn remote_pr_list_returns_platform_rows_newest_first_after_two_requests() {
    let f = Fixture::new(true, None).await;
    {
        let mut state = f.remote.state.lock().unwrap();
        state.pull_request_list_reply = Some(json!([
            listed_pull_request(8, "First pull request"),
            listed_pull_request(3, "Second pull request")
        ]));
    }
    f.policy(&f.actor, "git.pr_list", "allow").await;
    let writes_before = f.remote.writes();
    let receipts_before = state_receipt_count(&f, &f.actor).await;
    let calls_before = state_calls(&f).len();

    let result = f.call(&f.actor, "git.pr_list", json!({})).await.unwrap();

    assert_eq!(result["repo"], f.repo.display().to_string());
    assert_eq!(result["gate"]["decision"], "allow");
    assert_eq!(result["slug"], SLUG);
    assert_eq!(result["state"], "open");
    assert_eq!(
        result["pull_requests"],
        json!([
            {
                "number":8,
                "title":"First pull request",
                "state":"open",
                "draft":false,
                "author":"pull-author",
                "head_ref":"feature-8",
                "head_sha":"a".repeat(40),
                "base_ref":"main",
                "created_at":"2026-11-20T10:00:00Z",
                "updated_at":"2026-11-21T11:00:00Z"
            },
            {
                "number":3,
                "title":"Second pull request",
                "state":"open",
                "draft":false,
                "author":"pull-author",
                "head_ref":"feature-3",
                "head_sha":"a".repeat(40),
                "base_ref":"main",
                "created_at":"2026-11-20T10:00:00Z",
                "updated_at":"2026-11-21T11:00:00Z"
            }
        ])
    );
    assert_eq!(result["truncated"], false);
    assert_eq!(result["text_altered"], false);
    assert_eq!(result["trust"], "untrusted");
    let calls = state_calls(&f)[calls_before..].to_vec();
    assert_eq!(calls.len(), 2);
    assert!(calls.iter().all(|call| call["method"] == "GET"));
    assert_eq!(calls[0]["path"], format!("repos/{SLUG}"));
    assert_eq!(calls[1]["path"], list_path("pulls", "open", 100, 1));
    assert_eq!(f.remote.writes(), writes_before);
    assert_eq!(state_receipt_count(&f, &f.actor).await, receipts_before);
}

#[tokio::test]
async fn remote_lists_accept_all_and_reject_other_states_before_reading() {
    let f = Fixture::new(true, None).await;
    for verb in ["git.pr_list", "git.issue_list"] {
        let error = f
            .call(&f.actor, verb, json!({"state":"merged"}))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("invalid_params"), "{verb}: {error}");
    }
    assert!(state_calls(&f).is_empty());
    assert!(!f.dir.path().join("reads").exists());

    f.policy(&f.actor, "git.pr_list", "allow").await;
    f.policy(&f.actor, "git.issue_list", "allow").await;
    let calls_before = state_calls(&f).len();
    f.call(&f.actor, "git.pr_list", json!({"state":"all"}))
        .await
        .unwrap();
    f.call(&f.actor, "git.issue_list", json!({"state":"closed"}))
        .await
        .unwrap();
    let calls = state_calls(&f)[calls_before..].to_vec();
    assert_eq!(calls.len(), 4);
    assert_eq!(calls[1]["path"], list_path("pulls", "all", 100, 1));
    assert_eq!(calls[3]["path"], list_path("issues", "closed", 100, 1));
}

#[tokio::test]
async fn remote_issue_list_omits_pull_requests_and_preserves_null_author() {
    let f = Fixture::new(true, None).await;
    {
        let mut state = f.remote.state.lock().unwrap();
        state.issue_list_reply = Some(json!([
            listed_issue(4, "First issue", None, false),
            listed_issue(8, "Pull request issue entry", Some("pull-author"), true),
            listed_issue(5, "Second issue", Some("issue-author"), false)
        ]));
    }
    f.policy(&f.actor, "git.issue_list", "allow").await;
    let writes_before = f.remote.writes();
    let receipts_before = state_receipt_count(&f, &f.actor).await;
    let calls_before = state_calls(&f).len();

    let result = f
        .call(&f.actor, "git.issue_list", json!({}))
        .await
        .unwrap();

    assert_eq!(result["state"], "open");
    assert_eq!(
        result["issues"],
        json!([
            {
                "number":4,
                "title":"First issue",
                "state":"open",
                "author":null,
                "labels":["bug"],
                "comments":3,
                "created_at":"2026-11-20T10:00:00Z",
                "updated_at":"2026-11-21T11:00:00Z"
            },
            {
                "number":5,
                "title":"Second issue",
                "state":"open",
                "author":"issue-author",
                "labels":["bug"],
                "comments":3,
                "created_at":"2026-11-20T10:00:00Z",
                "updated_at":"2026-11-21T11:00:00Z"
            }
        ])
    );
    assert_eq!(result["pull_requests_omitted"], 1);
    assert_eq!(result["truncated"], false);
    assert_eq!(result["text_altered"], false);
    assert_eq!(result["trust"], "untrusted");
    let calls = state_calls(&f)[calls_before..].to_vec();
    assert_eq!(calls.len(), 2);
    assert!(calls.iter().all(|call| call["method"] == "GET"));
    assert_eq!(calls[1]["path"], list_path("issues", "open", 100, 1));
    assert_eq!(f.remote.writes(), writes_before);
    assert_eq!(state_receipt_count(&f, &f.actor).await, receipts_before);
}

#[tokio::test]
async fn remote_lists_read_every_page_up_to_ten() {
    let f = Fixture::new(true, None).await;
    {
        let mut state = f.remote.state.lock().unwrap();
        state.pull_request_list_reply = Some(json!((1..=250)
            .rev()
            .map(|number| listed_pull_request(number, "Pull request"))
            .collect::<Vec<_>>()));
        state.issue_list_reply = Some(json!((1..=1000)
            .rev()
            .map(|number| listed_issue(number, "Issue", Some("author"), false))
            .collect::<Vec<_>>()));
    }
    f.policy(&f.actor, "git.pr_list", "allow").await;
    f.policy(&f.actor, "git.issue_list", "allow").await;

    let calls_before = state_calls(&f).len();
    let pull_requests = f.call(&f.actor, "git.pr_list", json!({})).await.unwrap();
    let rows = pull_requests["pull_requests"].as_array().unwrap();
    assert_eq!(rows.len(), 250);
    assert_eq!(rows[0]["number"], 250);
    assert_eq!(rows[249]["number"], 1);
    assert_eq!(pull_requests["truncated"], false);
    let calls = state_calls(&f)[calls_before..].to_vec();
    assert_eq!(calls.len(), 4);
    assert_eq!(calls[3]["path"], list_path("pulls", "open", 100, 3));

    // Exactly ten full pages: the request for the next single row finds none.
    let calls_before = state_calls(&f).len();
    let issues = f.call(&f.actor, "git.issue_list", json!({})).await.unwrap();
    assert_eq!(issues["issues"].as_array().unwrap().len(), 1000);
    assert_eq!(issues["truncated"], false);
    let calls = state_calls(&f)[calls_before..].to_vec();
    assert_eq!(calls.len(), 12);
    assert_eq!(calls[10]["path"], list_path("issues", "open", 100, 10));
    assert_eq!(calls[11]["path"], list_path("issues", "open", 1, 1001));
}

#[tokio::test]
async fn remote_lists_mark_a_listing_past_ten_pages_as_truncated() {
    let f = Fixture::new(true, None).await;
    {
        let mut state = f.remote.state.lock().unwrap();
        state.pull_request_list_reply = Some(json!((1..=1001)
            .map(|number| listed_pull_request(number, "Pull request"))
            .collect::<Vec<_>>()));
        state.issue_list_reply = Some(json!((1..=1001)
            .map(|number| listed_issue(number, "Issue", Some("author"), number % 2 == 0))
            .collect::<Vec<_>>()));
    }
    f.policy(&f.actor, "git.pr_list", "allow").await;
    f.policy(&f.actor, "git.issue_list", "allow").await;

    let calls_before = state_calls(&f).len();
    let pull_requests = f.call(&f.actor, "git.pr_list", json!({})).await.unwrap();
    let rows = pull_requests["pull_requests"].as_array().unwrap();
    assert_eq!(rows.len(), 1000);
    assert_eq!(rows[999]["number"], 1000);
    assert_eq!(pull_requests["truncated"], true);
    assert_eq!(state_calls(&f).len() - calls_before, 12);

    // Pull request entries count toward the ten pages before they are omitted.
    let issues = f.call(&f.actor, "git.issue_list", json!({})).await.unwrap();
    assert_eq!(issues["issues"].as_array().unwrap().len(), 500);
    assert_eq!(issues["pull_requests_omitted"], 500);
    assert_eq!(issues["truncated"], true);
}

#[tokio::test]
async fn remote_lists_remove_hidden_characters_and_say_so() {
    let f = Fixture::new(true, None).await;
    {
        let mut state = f.remote.state.lock().unwrap();
        let mut pull_request = listed_pull_request(8, "Fix\u{202e}txt.exe");
        pull_request["head"]["ref"] = json!("feature\u{2066}-8");
        state.pull_request_list_reply = Some(json!([pull_request]));
        let mut issue = listed_issue(4, "Caf\u{e9} \u{6587}\u{6863}\u{feff}", Some("author"), false);
        issue["labels"] = json!([{"name":"bug\u{2067}"}]);
        state.issue_list_reply = Some(json!([issue]));
    }
    f.policy(&f.actor, "git.pr_list", "allow").await;
    f.policy(&f.actor, "git.issue_list", "allow").await;

    let pull_requests = f.call(&f.actor, "git.pr_list", json!({})).await.unwrap();
    assert_eq!(pull_requests["pull_requests"][0]["title"], "Fixtxt.exe");
    assert_eq!(pull_requests["pull_requests"][0]["head_ref"], "feature-8");
    assert_eq!(pull_requests["text_altered"], true);
    assert_eq!(pull_requests["trust"], "untrusted");

    let issues = f.call(&f.actor, "git.issue_list", json!({})).await.unwrap();
    assert_eq!(issues["issues"][0]["title"], "Caf\u{e9} \u{6587}\u{6863}");
    assert_eq!(issues["issues"][0]["labels"], json!(["bug"]));
    assert_eq!(issues["text_altered"], true);
}

#[tokio::test]
async fn remote_lists_reject_extra_arguments_before_reading() {
    let f = Fixture::new(true, None).await;
    for verb in ["git.pr_list", "git.issue_list"] {
        let error = f
            .call(&f.actor, verb, json!({"state":"open","extra":true}))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("unknown field `extra`"), "{verb}: {error}");
    }
    assert!(state_calls(&f).is_empty());
    assert!(!f.dir.path().join("reads").exists());
}

#[tokio::test]
async fn remote_lists_use_their_own_policy_decisions() {
    let f = Fixture::new(true, None).await;
    f.policy(&f.actor, "git.pr_list", "deny").await;
    f.policy(&f.actor, "git.pr_state", "allow").await;
    f.policy(&f.actor, "git.issue_list", "deny").await;
    f.policy(&f.actor, "git.issue", "allow").await;

    for verb in ["git.pr_list", "git.issue_list"] {
        let error = f
            .call(&f.actor, verb, json!({}))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("policy_denied"), "{verb}: {error}");
    }
    assert!(state_calls(&f).is_empty());
    assert!(!f.dir.path().join("reads").exists());
}

#[tokio::test]
async fn remote_lists_refuse_malformed_platform_answers() {
    let f = Fixture::new(true, None).await;
    {
        let mut state = f.remote.state.lock().unwrap();
        state.pull_request_list_reply = Some(json!({"items":[]}));
        let mut issue = listed_issue(1, "Labels not a list", Some("author"), false);
        issue["labels"] = json!("bug");
        state.issue_list_reply = Some(json!([issue]));
    }
    f.policy(&f.actor, "git.pr_list", "allow").await;
    f.policy(&f.actor, "git.issue_list", "allow").await;

    for verb in ["git.pr_list", "git.issue_list"] {
        let error = f
            .call(&f.actor, verb, json!({}))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("remote_response"), "{verb}: {error}");
    }
}

#[tokio::test]
async fn remote_lists_refuse_a_repository_answering_under_another_name_after_one_request() {
    let f = Fixture::new(true, None).await;
    f.remote.state.lock().unwrap().mismatch = Some("slug");
    f.policy(&f.actor, "git.pr_list", "allow").await;
    f.policy(&f.actor, "git.issue_list", "allow").await;
    for verb in ["git.pr_list", "git.issue_list"] {
        let calls_before = state_calls(&f).len();
        let error = f
            .call(&f.actor, verb, json!({}))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("repository_identity_mismatch"), "{verb}: {error}");
        assert_eq!(state_calls(&f).len() - calls_before, 1);
    }
    // Control: the same calls read once the platform answers under the configured name.
    f.remote.state.lock().unwrap().mismatch = None;
    for verb in ["git.pr_list", "git.issue_list"] {
        f.call(&f.actor, verb, json!({})).await.unwrap();
    }
}
