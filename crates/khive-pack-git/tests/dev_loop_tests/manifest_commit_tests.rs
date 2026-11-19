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
