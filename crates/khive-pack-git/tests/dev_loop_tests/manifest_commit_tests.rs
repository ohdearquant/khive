#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn manifest_commit_preserves_symlink_mode_and_literal_target() {
    if crate::test_process::run_in_child() {
        return;
    }

    let f = Fixture::new(true, true).await;
    f.policy("git.commit", "allow").await;
    let index = std::fs::read(f.repo.join(".git/index")).expect("index before");
    let worktree = std::fs::read(f.repo.join("a.txt")).expect("worktree before");
    let target = b"../missing-\xff\n";
    let manifest = f.tree(&[("link", target, 120000)]).await;
    let result = f.call("git.commit", f.commit_params(&manifest)).await;
    let sha = result["sha"].as_str().expect("commit SHA");
    assert_eq!(f.git_text(&["rev-parse", &format!("{sha}^")]), f.base);
    assert_eq!(
        f.git_bytes(&["ls-tree", "-r", "--name-only", sha]),
        b"link\n"
    );
    assert!(f
        .git_text(&["ls-tree", "-r", sha, "link"])
        .starts_with("120000 blob "));
    assert_eq!(
        f.git_bytes(&["cat-file", "blob", &format!("{sha}:link")]),
        target
    );
    assert_eq!(f.git_text(&["rev-parse", "refs/heads/work"]), sha);
    assert_eq!(std::fs::read(f.repo.join(".git/index")).unwrap(), index);
    assert_eq!(std::fs::read(f.repo.join("a.txt")).unwrap(), worktree);
    assert!(std::fs::symlink_metadata(f.repo.join("link")).is_err());
    f.success_receipt(&result).await;
    let error = f
        .err("git.checkout", json!({"repo":f.repo,"ref":sha}))
        .await;
    assert!(error.contains("unsupported_entry"), "{error}");
    let receipt = f.refusal_receipt(&error).await;
    assert_eq!(receipt["verb"], "git.checkout");
    assert_eq!(receipt["reason"], "unsupported_entry");
}

#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn arm14_manifest_commit_preserves_index_and_worktree_residue() {
    if crate::test_process::run_in_child() {
        return;
    }

    let f = Fixture::new(true, true).await;
    f.policy("git.commit", "allow").await;
    std::fs::write(f.repo.join("a.txt"), b"staged\n").expect("staged content");
    f.git_bytes(&["add", "--", "a.txt"]);
    std::fs::write(f.repo.join("a.txt"), b"unstaged\n").expect("worktree residue");
    std::fs::write(f.repo.join("untracked"), b"untracked\n").expect("untracked residue");
    let index = std::fs::read(f.repo.join(".git/index")).expect("index bytes");
    let manifest = f
        .tree(&[
            ("a.txt", b"manifest\n", 644),
            ("nested/run", b"#!/bin/sh\n", 755),
        ])
        .await;
    let result = f.call("git.commit", f.commit_params(&manifest)).await;
    let sha = result["sha"].as_str().expect("commit SHA");
    assert_eq!(f.git_text(&["rev-parse", &format!("{sha}^")]), f.base);
    assert_eq!(
        f.git_bytes(&["ls-tree", "-r", "--name-only", sha]),
        b"a.txt\nnested/run\n"
    );
    assert_eq!(
        f.git_bytes(&["cat-file", "blob", &format!("{sha}:a.txt")]),
        b"manifest\n"
    );
    assert!(f
        .git_text(&["ls-tree", "-r", sha, "nested/run"])
        .starts_with("100755 blob "));
    assert_eq!(
        std::fs::read(f.repo.join(".git/index")).expect("index after"),
        index
    );
    assert_eq!(
        std::fs::read(f.repo.join("a.txt")).expect("worktree after"),
        b"unstaged\n"
    );
    assert_eq!(
        std::fs::read(f.repo.join("untracked")).expect("untracked after"),
        b"untracked\n"
    );
    assert_eq!(
        f.git_text(&["show", "-s", "--format=%an <%ae>", sha]),
        "Mapped Author <mapped@example.invalid>"
    );
    let checkout = f
        .call("git.checkout", json!({"repo":f.repo,"ref":sha}))
        .await;
    assert_eq!(
        checkout["tree"], manifest,
        "exact complete manifest including modes and deletion"
    );
    let row = f.success_receipt(&result).await;
    assert_eq!(
        row["credential"],
        json!({"source":"actor","ref":"fixture-reference","platform_identity":"fixture-login"})
    );
    assert_eq!(f.resolver_count(), 1);
}

#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn merge_parent_commit_records_branch_head_first_and_published_tip_second() {
    if crate::test_process::run_in_child() {
        return;
    }

    let f = Fixture::new(true, true).await;
    f.policy("git.commit", "allow").await;
    let base_tree = f.git_text(&["rev-parse", "HEAD^{tree}"]);
    let upstream = f.git_text(&["commit-tree", &base_tree, "-p", &f.base, "-m", "upstream"]);
    f.git_bytes(&["update-ref", "refs/remotes/origin/main", &upstream]);
    let manifest = f.tree(&[("a.txt", b"resolved\n", 644)]).await;
    let mut params = f.commit_params(&manifest);
    params["merge_parent"] = json!(upstream);
    let result = f.call("git.commit", params).await;
    let sha = result["sha"].as_str().expect("commit SHA");
    assert_eq!(
        f.git_text(&["rev-list", "--parents", "-n", "1", sha]),
        format!("{sha} {} {upstream}", f.base),
        "the branch head is the first parent and the published tip the second"
    );
    assert_eq!(result["parent"], f.base);
    assert_eq!(result["merge_parent"], upstream);
    assert_eq!(f.git_text(&["rev-parse", "refs/heads/work"]), sha);
    assert_eq!(
        f.git_bytes(&["ls-tree", "-r", "--name-only", sha]),
        b"a.txt\n"
    );
    assert_eq!(
        f.git_bytes(&["cat-file", "blob", &format!("{sha}:a.txt")]),
        b"resolved\n"
    );
    let row = f.success_receipt(&result).await;
    assert_eq!(row["inputs"]["merge_parent"], upstream);

    // A one-parent commit's result carries no merge_parent field at all.
    let next = f.tree(&[("a.txt", b"after\n", 644)]).await;
    let mut params = f.commit_params(&next);
    params["expected_head"] = json!(sha);
    let plain = f.call("git.commit", params).await;
    assert!(plain.get("merge_parent").is_none(), "{plain}");
}

#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn merge_parent_must_be_the_tip_of_a_remote_tracking_ref() {
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

    // A local branch does not make a commit published.
    f.git_bytes(&["update-ref", "refs/heads/side", &side]);
    let refs = f.git_bytes(&["show-ref"]);
    let error = f.err("git.commit", params.clone()).await;
    assert!(error.contains("merge_parent_unpublished"), "{error}");
    assert_eq!(
        f.refusal_receipt(&error).await["reason"],
        "merge_parent_unpublished"
    );
    assert_eq!(f.git_bytes(&["show-ref"]), refs);
    assert_eq!(f.resolver_count(), 0);

    // Reachable from a remote-tracking ref without being its tip still refuses.
    let ahead = f.git_text(&["commit-tree", &base_tree, "-p", &side, "-m", "ahead"]);
    f.git_bytes(&["update-ref", "refs/remotes/origin/main", &ahead]);
    let refs = f.git_bytes(&["show-ref"]);
    let error = f.err("git.commit", params.clone()).await;
    assert!(error.contains("merge_parent_unpublished"), "{error}");
    assert_eq!(f.git_bytes(&["show-ref"]), refs);
    assert_eq!(f.resolver_count(), 0);

    // Control: the same call commits once the commit is a remote-tracking tip.
    f.git_bytes(&["update-ref", "refs/remotes/origin/side", &side]);
    let result = f.call("git.commit", params).await;
    assert_eq!(result["merge_parent"], side);
}

#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn merge_parent_refuses_malformed_merged_and_paths_form_values() {
    if crate::test_process::run_in_child() {
        return;
    }

    let f = Fixture::new(true, true).await;
    f.policy("git.commit", "allow").await;
    f.git_bytes(&["update-ref", "refs/remotes/origin/main", &f.base]);
    let base_tree = f.git_text(&["rev-parse", "HEAD^{tree}"]);
    let manifest = f.tree(&[("a.txt", b"resolved\n", 644)]).await;
    for (value, reason) in [
        (json!(null), "invalid_params"),
        (json!("abc"), "invalid_params"),
        (json!("0".repeat(40)), "invalid_params"),
        (json!("1".repeat(40)), "invalid_params"),
        (json!(base_tree), "invalid_params"),
        (json!(f.base), "merge_parent_already_merged"),
    ] {
        let mut params = f.commit_params(&manifest);
        params["merge_parent"] = value.clone();
        let error = f.err("git.commit", params).await;
        assert!(error.contains(reason), "{value}: {error}");
        assert_eq!(f.git_text(&["rev-parse", "refs/heads/work"]), f.base);
    }
    assert_eq!(f.resolver_count(), 0);

    let error = f
        .err(
            "git.commit",
            json!({"repo":f.repo,"paths":["a.txt"],"message":"legacy paths commit","merge_parent":f.base}),
        )
        .await;
    assert!(error.contains("invalid_params"), "{error}");
    assert_eq!(f.git_text(&["rev-parse", "refs/heads/work"]), f.base);
}

#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn a_repository_author_replaces_the_actor_identity_and_the_actor_still_authorizes() {
    if crate::test_process::run_in_child() {
        return;
    }

    let f = Fixture::with_repository_row(Some(("Repository Author", "repository@example.invalid")))
        .await;
    f.policy("git.commit", "allow").await;
    let manifest = f.tree(&[("a.txt", b"repository identity\n", 644)]).await;
    let result = f.call("git.commit", f.commit_params(&manifest)).await;
    let sha = result["sha"].as_str().expect("commit SHA");
    assert_eq!(
        f.git_text(&["show", "-s", "--format=%an <%ae>|%cn <%ce>", sha]),
        "Repository Author <repository@example.invalid>|Repository Author <repository@example.invalid>"
    );
    assert_eq!(
        result["author_source"],
        json!({"table":"git_write.repositories","key":f.repo.display().to_string()})
    );
    let row = f.success_receipt(&result).await;
    assert_eq!(
        row["credential"],
        json!({"source":"actor","ref":"fixture-reference","platform_identity":"fixture-login"}),
        "the actor's row still authorizes the commit"
    );
    assert_eq!(f.resolver_count(), 1);
}

#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn a_repository_row_without_an_author_keeps_the_actor_identity() {
    if crate::test_process::run_in_child() {
        return;
    }

    let f = Fixture::with_repository_row(None).await;
    f.policy("git.commit", "allow").await;
    let manifest = f.tree(&[("a.txt", b"actor identity\n", 644)]).await;
    let result = f.call("git.commit", f.commit_params(&manifest)).await;
    let sha = result["sha"].as_str().expect("commit SHA");
    assert_eq!(
        f.git_text(&["show", "-s", "--format=%an <%ae>|%cn <%ce>", sha]),
        "Mapped Author <mapped@example.invalid>|Mapped Author <mapped@example.invalid>"
    );
    assert!(result.get("author_source").is_none(), "{result}");
    f.success_receipt(&result).await;
}
