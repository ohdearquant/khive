fn issue_payload(body: Option<&str>, comments: usize) -> Value {
    let mut issue = json!({
        "title": "Repository issue",
        "state": "open",
        "user": {"login": "issue-author"},
        "created_at": "2026-11-20T10:00:00Z",
        "updated_at": "2026-11-21T11:00:00Z",
        "labels": [{"name": "bug"}, {"name": "priority"}],
        "comments": comments,
    });
    if let Some(body) = body {
        issue["body"] = json!(body);
    }
    issue
}

fn issue_comment(author: &str, created_at: &str, body: &str) -> Value {
    json!({"user":{"login":author},"created_at":created_at,"body":body})
}

#[tokio::test]
async fn remote_issue_returns_platform_fields_and_ordered_comments() {
    let f = Fixture::new(true, None).await;
    let number = 42;
    let body = "First line\nSecond line stays whole.";
    {
        let mut state = f.remote.state.lock().unwrap();
        state.issues.insert(number, issue_payload(Some(body), 2));
        state.issue_comments.insert(
            number,
            vec![
                issue_comment("first-commenter", "2026-11-20T12:00:00Z", "First reply."),
                issue_comment("second-commenter", "2026-11-21T12:00:00Z", "Second reply."),
            ],
        );
    }
    f.policy(&f.actor, "git.issue", "allow").await;
    let writes_before = f.remote.writes();
    let receipts_before = state_receipt_count(&f, &f.actor).await;
    let calls_before = state_calls(&f).len();

    let issue = f
        .call(&f.actor, "git.issue", json!({"number":number}))
        .await
        .unwrap();

    assert_eq!(issue["repo"], f.repo.display().to_string());
    assert_eq!(issue["gate"]["decision"], "allow");
    assert_eq!(issue["slug"], SLUG);
    assert_eq!(issue["number"], number);
    assert_eq!(issue["title"], "Repository issue");
    assert_eq!(issue["state"], "open");
    assert_eq!(issue["author"], "issue-author");
    assert_eq!(issue["created_at"], "2026-11-20T10:00:00Z");
    assert_eq!(issue["updated_at"], "2026-11-21T11:00:00Z");
    assert_eq!(issue["labels"], json!(["bug", "priority"]));
    assert_eq!(issue["body"], body);
    assert_eq!(issue["is_pull_request"], false);
    assert_eq!(
        issue["comments"],
        json!([
            {"author":"first-commenter","created_at":"2026-11-20T12:00:00Z","body":"First reply."},
            {"author":"second-commenter","created_at":"2026-11-21T12:00:00Z","body":"Second reply."},
        ])
    );
    assert_eq!(issue["comments_total"], 2);
    assert_eq!(issue["comments_truncated"], false);
    assert_eq!(issue["text_altered"], false);
    assert_eq!(issue["trust"], "untrusted");
    assert_eq!(f.remote.writes(), writes_before);
    assert_eq!(state_receipt_count(&f, &f.actor).await, receipts_before);
    let calls = state_calls(&f)[calls_before..].to_vec();
    assert_eq!(calls.len(), 3);
    assert!(calls.iter().all(|call| call["method"] == "GET"));
    assert_eq!(calls[0]["path"], format!("repos/{SLUG}"));
    assert_eq!(calls[1]["path"], format!("repos/{SLUG}/issues/{number}"));
    assert_eq!(
        calls[2]["path"],
        format!("repos/{SLUG}/issues/{number}/comments?per_page=100&page=1")
    );
}

#[tokio::test]
async fn remote_issue_marks_a_pull_request_number() {
    let f = Fixture::new(true, None).await;
    let number = f.open().await;
    f.policy(&f.actor, "git.issue", "allow").await;

    let issue = f
        .call(&f.actor, "git.issue", json!({"number":number}))
        .await
        .unwrap();

    assert_eq!(issue["is_pull_request"], true);
}

#[tokio::test]
async fn remote_issue_refuses_an_unknown_number() {
    let f = Fixture::new(true, None).await;
    f.policy(&f.actor, "git.issue", "allow").await;

    let error = f
        .call(&f.actor, "git.issue", json!({"number":99}))
        .await
        .unwrap_err()
        .to_string();

    assert!(error.contains("remote_refused"), "{error}");
}

#[tokio::test]
async fn remote_issue_rejects_extra_missing_and_nonpositive_numbers() {
    let f = Fixture::new(true, None).await;
    f.policy(&f.actor, "git.issue", "allow").await;
    let calls_before = state_calls(&f).len();

    for (params, expected) in [
        (json!({"number":1,"extra":true}), "unknown field `extra`"),
        (json!({}), "invalid_params"),
        (json!({"number":0}), "invalid_params"),
    ] {
        let error = f
            .call(&f.actor, "git.issue", params)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains(expected), "expected {expected}: {error}");
    }

    assert_eq!(state_calls(&f).len(), calls_before);
    assert!(!f.dir.path().join("reads").exists());
}

#[tokio::test]
async fn remote_issue_uses_its_own_policy_decision() {
    let f = Fixture::new(true, None).await;
    f.policy(&f.actor, "git.issue", "deny").await;
    f.policy(&f.actor, "git.pr_state", "allow").await;
    let calls_before = state_calls(&f).len();

    let error = f
        .call(&f.actor, "git.issue", json!({"number":1}))
        .await
        .unwrap_err()
        .to_string();

    assert!(error.contains("policy_denied"), "{error}");
    assert_eq!(state_calls(&f).len(), calls_before);
    assert!(!f.dir.path().join("reads").exists());
}

fn seed_comments(f: &Fixture, number: u64, count: usize) {
    let mut state = f.remote.state.lock().unwrap();
    state
        .issues
        .insert(number, issue_payload(Some("Issue body."), count));
    state.issue_comments.insert(
        number,
        (0..count)
            .map(|index| {
                issue_comment(
                    "commenter",
                    "2026-11-21T12:00:00Z",
                    &format!("Reply {index}"),
                )
            })
            .collect(),
    );
}

#[tokio::test]
async fn remote_issue_reads_every_comment_page() {
    let f = Fixture::new(true, None).await;
    let number = 43;
    seed_comments(&f, number, 125);
    f.policy(&f.actor, "git.issue", "allow").await;
    let calls_before = state_calls(&f).len();

    let issue = f
        .call(&f.actor, "git.issue", json!({"number":number}))
        .await
        .unwrap();

    assert_eq!(issue["comments"].as_array().unwrap().len(), 125);
    assert_eq!(issue["comments"][0]["body"], "Reply 0");
    assert_eq!(issue["comments"][100]["body"], "Reply 100");
    assert_eq!(issue["comments"][124]["body"], "Reply 124");
    assert_eq!(issue["comments_total"], 125);
    assert_eq!(issue["comments_truncated"], false);
    let calls = state_calls(&f)[calls_before..].to_vec();
    assert_eq!(calls.len(), 4);
    assert_eq!(
        calls[3]["path"],
        format!("repos/{SLUG}/issues/{number}/comments?per_page=100&page=2")
    );
}

#[tokio::test]
async fn remote_issue_stops_at_ten_comment_pages_and_says_so() {
    let f = Fixture::new(true, None).await;
    let number = 45;
    seed_comments(&f, number, 1001);
    f.policy(&f.actor, "git.issue", "allow").await;
    let calls_before = state_calls(&f).len();

    let issue = f
        .call(&f.actor, "git.issue", json!({"number":number}))
        .await
        .unwrap();

    assert_eq!(issue["comments"].as_array().unwrap().len(), 1000);
    assert_eq!(issue["comments"][999]["body"], "Reply 999");
    assert_eq!(issue["comments_total"], 1001);
    assert_eq!(issue["comments_truncated"], true);
    assert_eq!(state_calls(&f)[calls_before..].len(), 12);
}

#[tokio::test]
async fn remote_issue_removes_hidden_characters_and_escapes_and_says_so() {
    let f = Fixture::new(true, None).await;
    let number = 46;
    {
        let mut state = f.remote.state.lock().unwrap();
        let mut issue = issue_payload(
            Some("\u{1b}[31mred\u{1b}[0m\nline\u{200B} two\tend"),
            1,
        );
        issue["title"] = json!("Title\u{202E}reversed");
        issue["labels"] = json!([{"name": "bu\u{2066}g"}]);
        state.issues.insert(number, issue);
        state.issue_comments.insert(
            number,
            vec![issue_comment(
                "commenter",
                "2026-11-21T12:00:00Z",
                "keep \u{00e9}t\u{00e9} \u{6f22}\u{5b57}\u{feff}",
            )],
        );
    }
    f.policy(&f.actor, "git.issue", "allow").await;

    let issue = f
        .call(&f.actor, "git.issue", json!({"number":number}))
        .await
        .unwrap();

    assert_eq!(issue["title"], "Titlereversed");
    assert_eq!(issue["labels"], json!(["bug"]));
    assert_eq!(issue["body"], "red\nline two\tend");
    assert_eq!(
        issue["comments"][0]["body"],
        "keep \u{00e9}t\u{00e9} \u{6f22}\u{5b57}"
    );
    assert_eq!(issue["text_altered"], true);
}

#[tokio::test]
async fn remote_issue_checks_the_repository_and_the_allowance_first() {
    let f = Fixture::new(true, None).await;
    let number = 47;
    seed_comments(&f, number, 1);
    f.policy(&f.actor, "git.issue", "allow").await;
    type Arm = (fn(&mut State), &'static str);
    let arms: [Arm; 3] = [
        (|s| s.mismatch = Some("slug"), "repository_identity_mismatch"),
        (
            |s| s.mismatch = Some("visibility"),
            "repository_identity_mismatch",
        ),
        (
            |s| s.rate_limited = Some(1_795_346_246),
            "remote_rate_limited: the allowance returns at epoch second 1795346246",
        ),
    ];
    for (arm, expected) in arms {
        {
            let mut s = f.remote.state.lock().unwrap();
            s.mismatch = None;
            s.rate_limited = None;
            arm(&mut s);
        }
        let calls_before = state_calls(&f).len();
        let error = f
            .call(&f.actor, "git.issue", json!({"number":number}))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains(expected), "expected {expected}: {error}");
        assert_eq!(state_calls(&f).len(), calls_before + 1, "{expected}");
    }
    {
        let mut s = f.remote.state.lock().unwrap();
        s.mismatch = None;
        s.rate_limited = None;
    }
    let issue = f
        .call(&f.actor, "git.issue", json!({"number":number}))
        .await
        .unwrap();
    assert_eq!(issue["comments_total"], 1);
}

#[tokio::test]
async fn remote_issue_without_a_body_returns_an_empty_string() {
    let f = Fixture::new(true, None).await;
    let number = 44;
    {
        let mut state = f.remote.state.lock().unwrap();
        state.issues.insert(number, issue_payload(None, 0));
    }
    f.policy(&f.actor, "git.issue", "allow").await;

    let issue = f
        .call(&f.actor, "git.issue", json!({"number":number}))
        .await
        .unwrap();

    assert_eq!(issue["body"], "");
}
