/// #2539: a refusal from a real non-zero `git` exit used to be the same four
/// words as one from an unparseable record, so a flake on a runner taught
/// nothing. The classification still leads; the exit status follows it.
#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn log_refusal_names_the_git_exit_status() {
    if crate::test_process::run_in_child() {
        return;
    }

    let f = Fixture::new(true, true).await;
    f.policy("git.log", "allow").await;
    let control = f.call("git.log", json!({"repo": f.repo, "limit": 1})).await;
    assert!(
        control["commits"].as_array().is_some_and(|c| !c.is_empty()),
        "control: the same verb reads the repository fine: {control}"
    );

    let error = f
        .err(
            "git.log",
            json!({"repo": f.repo, "ref": "refs/heads/absent-on-purpose"}),
        )
        .await;
    assert!(
        error.contains("git_failed"),
        "#2539: the classification still leads the message; got {error}"
    );
    assert!(
        error.contains("exit status"),
        "#2539: the refusal must name git's exit status; got {error}"
    );
}

#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn log_agrees_with_native_rev_list_and_bounds_its_page() {
    if crate::test_process::run_in_child() {
        return;
    }

    let f = Fixture::new(true, true).await;
    f.policy("git.log", "allow").await;
    let page = f
        .call("git.log", json!({"repo": f.repo, "limit": 10}))
        .await;
    let mine: Vec<String> = page["commits"]
        .as_array()
        .expect("commits array")
        .iter()
        .map(|c| c["sha"].as_str().expect("sha").to_string())
        .collect();
    let native: Vec<String> = f
        .git_text(&["rev-list", "-n", "10", "HEAD"])
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(mine, native, "{page}");
    let head = &page["commits"][0];
    assert_eq!(head["sha"], json!(f.base));
    assert!(
        head["subject"].as_str().is_some_and(|s| !s.is_empty()),
        "subject must be populated: {head}"
    );
    assert!(
        head["committed_at"]
            .as_str()
            .is_some_and(|s| s.contains('T')),
        "committed_at must be ISO 8601: {head}"
    );

    let one = f.call("git.log", json!({"repo": f.repo, "limit": 1})).await;
    assert_eq!(one["commits"].as_array().expect("commits").len(), 1);
    assert_eq!(one["truncated"], json!(true));
    for bad in [json!(0), json!(501), json!("10"), json!(null)] {
        let error = f
            .err("git.log", json!({"repo": f.repo, "limit": bad}))
            .await;
        assert!(error.contains("invalid_params"), "limit {bad}: {error}");
    }
}

#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn log_treats_a_path_filter_literally_rather_than_as_a_glob() {
    if crate::test_process::run_in_child() {
        return;
    }

    let f = Fixture::new(true, true).await;
    f.policy("git.log", "allow").await;
    std::fs::write(f.repo.join("*.txt"), b"star\n").expect("a file literally named *.txt");
    output(
        command(&f.git, &f.repo, &["add", "--", "*.txt"], true),
        None,
    );
    output(
        command(
            &f.git,
            &f.repo,
            &["commit", "-q", "-m", "the literal star file"],
            true,
        ),
        None,
    );
    let literal = f
        .call(
            "git.log",
            json!({"repo": f.repo, "path": "*.txt", "limit": 50}),
        )
        .await;
    let commits = literal["commits"].as_array().expect("commits array");
    assert_eq!(
        commits.len(),
        1,
        "a glob would also match a.txt and removed.txt: {literal}"
    );
    assert_eq!(commits[0]["subject"], json!("the literal star file"));
}

#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn status_and_log_refuse_off_allowlist_and_on_deny_and_write_no_receipt() {
    if crate::test_process::run_in_child() {
        return;
    }

    // No status/log grant is made here on purpose. The allowlist is consulted before the policy,
    // so the off-allowlist arm below refuses without one, which is what proves that ordering.
    let f = Fixture::new(true, true).await;
    let before = f
        .call("git.receipts", json!({"limit":500,"offset":0}))
        .await["receipts"]
        .as_array()
        .expect("receipts array")
        .len();

    let outside = f.dir.path().join("not-listed");
    std::fs::create_dir(&outside).expect("unlisted directory");
    for verb in ["git.status", "git.log"] {
        let error = f.err(verb, json!({"repo": outside})).await;
        assert!(
            error.contains("repo_not_allowlisted"),
            "{verb} off the allowlist: {error}"
        );
    }
    // Control first, and it has to be first: tool policies are append-only rows and a deny is not
    // reversible by a later allow, so a control placed after the deny arm would fail on a healthy
    // pack. The same repo and the same call shape succeed while the decision is allow, which is
    // what makes the refusals below the policy's doing rather than a broken fixture.
    for verb in ["git.status", "git.log"] {
        f.policy(verb, "allow").await;
        f.call(verb, json!({"repo": f.repo})).await;
    }
    for verb in ["git.status", "git.log"] {
        f.policy(verb, "deny").await;
        let error = f.err(verb, json!({"repo": f.repo})).await;
        assert!(error.contains("policy_denied"), "{verb} denied: {error}");
    }

    let after = f
        .call("git.receipts", json!({"limit":500,"offset":0}))
        .await["receipts"]
        .as_array()
        .expect("receipts array")
        .len();
    assert_eq!(
        before, after,
        "git.status and git.log write no receipt, allowed or refused"
    );
}
