#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn checkout_names_not_fetched_for_an_absent_fetched_ref() {
    if crate::test_process::run_in_child() {
        return;
    }

    let f = Fixture::new(true, false).await;
    for (requested, reference) in [
        ("origin/feature", "refs/remotes/origin/feature"),
        ("refs/remotes/origin/feature", "refs/remotes/origin/feature"),
        ("refs/fetched/pull/7", "refs/fetched/pull/7"),
    ] {
        let error = f
            .err("git.checkout", json!({"repo":f.repo,"ref":requested}))
            .await;
        assert!(error.contains("not_fetched"), "{requested}: {error}");
        // Control: once the ref exists the same spelling checks out.
        f.git_bytes(&["update-ref", reference, &f.base]);
        let result = f
            .call("git.checkout", json!({"repo":f.repo,"ref":requested}))
            .await;
        assert_eq!(result["commit"], f.base);
        f.git_bytes(&["update-ref", "-d", reference]);
    }
    // A ref of another shape keeps its own refusal.
    let error = f
        .err("git.checkout", json!({"repo":f.repo,"ref":"no-such-branch"}))
        .await;
    assert!(!error.contains("not_fetched"), "{error}");
}

#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn checkout_refuses_the_fetch_staging_namespace_even_when_present() {
    if crate::test_process::run_in_child() {
        return;
    }

    let f = Fixture::new(true, false).await;
    f.git_bytes(&["update-ref", "refs/khive/fetch/staged", &f.base]);
    let error = f
        .err(
            "git.checkout",
            json!({"repo":f.repo,"ref":"refs/khive/fetch/staged"}),
        )
        .await;
    assert!(error.contains("invalid_params"), "{error}");
    // Control: the same commit by another name checks out.
    f.git_bytes(&["update-ref", "refs/remotes/origin/staged", &f.base]);
    let result = f
        .call(
            "git.checkout",
            json!({"repo":f.repo,"ref":"refs/remotes/origin/staged"}),
        )
        .await;
    assert_eq!(result["commit"], f.base);
}

#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn merge_parent_refuses_a_fetched_pull_head() {
    if crate::test_process::run_in_child() {
        return;
    }

    let f = Fixture::new(true, true).await;
    f.policy("git.commit", "allow").await;
    let base_tree = f.git_text(&["rev-parse", "HEAD^{tree}"]);
    let side = f.git_text(&["commit-tree", &base_tree, "-p", &f.base, "-m", "side"]);
    let manifest = f.tree(&[("a.txt", b"resolved\n", 644)]).await;
    let mut params = f.commit_params(&manifest);
    params["merge_parent"] = json!(side);

    // A fetched pull request head is not published, so it is not a merge parent.
    f.git_bytes(&["update-ref", "refs/fetched/pull/7", &side]);
    let refs = f.git_bytes(&["show-ref"]);
    let error = f.err("git.commit", params.clone()).await;
    assert!(error.contains("merge_parent_unpublished"), "{error}");
    assert_eq!(f.git_bytes(&["show-ref"]), refs);

    // Control: the same commit as a remote-tracking tip is a merge parent.
    f.git_bytes(&["update-ref", "refs/remotes/origin/side", &side]);
    let result = f.call("git.commit", params).await;
    assert_eq!(result["merge_parent"], side);
}
