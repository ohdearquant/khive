#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn checkout_preserves_status_index_and_raw_content() {
    if crate::test_process::run_in_child() {
        return;
    }

    let f = Fixture::new(true, false).await;
    std::fs::write(f.repo.join("a.txt"), b"staged\n").unwrap();
    f.git_bytes(&["add", "--", "a.txt"]);
    std::fs::write(f.repo.join("a.txt"), b"unstaged\n").unwrap();
    std::fs::write(f.repo.join("untracked"), b"untracked\n").unwrap();
    let status = f.git_bytes(&["status", "--porcelain=v1", "-z"]);
    let index = std::fs::read(f.repo.join(".git/index")).unwrap();
    let result = f
        .call("git.checkout", json!({"repo":f.repo,"ref":f.base}))
        .await;
    assert_eq!(result["commit"], f.base);
    assert_eq!(f.git_bytes(&["status", "--porcelain=v1", "-z"]), status);
    assert_eq!(std::fs::read(f.repo.join(".git/index")).unwrap(), index);
    let manifest = f
        .call("exec.tree_get", json!({"tree":result["tree"]}))
        .await;
    let entries = manifest["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    let entry = entries
        .iter()
        .find(|entry| entry["path"] == "a.txt")
        .unwrap();
    assert_eq!(f.blob(entry["ref"].as_str().unwrap()).await, b"old\n");
    f.success_receipt(&result).await;
}

#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn checkout_refuses_symlinks_and_submodules_without_checkout() {
    if crate::test_process::run_in_child() {
        return;
    }

    for entry_kind in ["symlink", "submodule"] {
        let f = Fixture::new(true, false).await;
        if entry_kind == "symlink" {
            std::os::unix::fs::symlink("a.txt", f.repo.join("link")).expect("symlink fixture");
            f.git_bytes(&["add", "--", "link"]);
        } else {
            f.git_bytes(&[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("160000,{},submodule", f.base),
            ]);
        }
        f.git_bytes(&["commit", "-q", "-m", "unsupported tree entry"]);
        let status = f.git_bytes(&["status", "--porcelain=v1", "-z"]);
        let index = std::fs::read(f.repo.join(".git/index")).expect("index");
        let error = f
            .err("git.checkout", json!({"repo":f.repo,"ref":"HEAD"}))
            .await;
        f.refusal_receipt(&error).await;
        assert_eq!(f.git_bytes(&["status", "--porcelain=v1", "-z"]), status);
        assert_eq!(
            std::fs::read(f.repo.join(".git/index")).expect("index after"),
            index
        );
    }
}

#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn diff_bytes_and_inputs_match_independent_native_oracle_for_commits_and_trees() {
    if crate::test_process::run_in_child() {
        return;
    }

    let f = Fixture::new(true, false).await;
    let base_tree = f
        .call("git.checkout", json!({"repo":f.repo,"ref":f.base}))
        .await["tree"]
        .clone();
    std::fs::write(f.repo.join("a.txt"), b"new\nsecond\n").unwrap();
    std::fs::remove_file(f.repo.join("removed.txt")).unwrap();
    f.git_bytes(&["add", "-A"]);
    f.git_bytes(&["commit", "-q", "-m", "diff head"]);
    let head = f.git_text(&["rev-parse", "HEAD"]);
    let head_tree = f
        .call("git.checkout", json!({"repo":f.repo,"ref":head}))
        .await["tree"]
        .clone();
    for (kind, base, target, oracle_base, oracle_head) in [
        (
            "commits",
            json!(f.base),
            json!(head),
            f.base.clone(),
            head.clone(),
        ),
        (
            "trees",
            base_tree,
            head_tree,
            f.git_text(&["rev-parse", &format!("{}^{{tree}}", f.base)]),
            f.git_text(&["rev-parse", "HEAD^{tree}"]),
        ),
    ] {
        let oracle = f.git_bytes(&[
            "diff-tree",
            "-p",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--no-renames",
            &oracle_base,
            &oracle_head,
        ]);
        let result = f
            .call(
                "git.diff",
                json!({"repo":f.repo,"input_kind":kind,"base":base,"head":target}),
            )
            .await;
        assert_eq!(f.blob(result["diff"].as_str().unwrap()).await, oracle);
        assert_eq!(
            result["summary"],
            json!({"files":2,"additions":2,"deletions":2})
        );
        let row = f.success_receipt(&result).await;
        assert_eq!(
            row["inputs"],
            json!({"input_kind":kind,"base":base,"head":target})
        );
    }
}

#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn tree_diff_symlink_changes_match_native_git_patches() {
    if crate::test_process::run_in_child() {
        return;
    }

    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::symlink;

    let f = Fixture::new(true, false).await;
    let mut base = f.indexed_tree().await;
    let mut base_git = f.git_text(&["write-tree"]);
    let unchanged = f
        .call(
            "git.diff",
            json!({"repo":f.repo,"input_kind":"trees","base":base,"head":base}),
        )
        .await;
    assert!(f.blob(unchanged["diff"].as_str().unwrap()).await.is_empty());
    assert_eq!(
        unchanged["summary"],
        json!({"files":0,"additions":0,"deletions":0})
    );

    for (change, mode, bytes) in [
        ("add", Some(120000), b"./a.txt".as_slice()),
        ("retarget", Some(120000), b"./removed.txt".as_slice()),
        ("symlink_to_file", Some(644), b"./removed.txt".as_slice()),
        ("file_to_symlink", Some(120000), b"./removed.txt".as_slice()),
        ("remove", None, b"".as_slice()),
    ] {
        let link = f.repo.join("link");
        if std::fs::symlink_metadata(&link).is_ok() {
            std::fs::remove_file(&link).expect("remove previous entry");
        }
        match mode {
            Some(120000) => symlink(OsStr::from_bytes(bytes), &link).expect("symlink"),
            Some(644) => std::fs::write(&link, bytes).expect("regular file"),
            None => {}
            _ => unreachable!("fixture modes are fixed"),
        }
        f.git_bytes(&["add", "--all"]);
        let head = f.indexed_tree().await;
        let head_git = f.git_text(&["write-tree"]);
        let oracle = f.git_bytes(&[
            "diff-tree",
            "-p",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--no-renames",
            &base_git,
            &head_git,
        ]);
        let result = f
            .call(
                "git.diff",
                json!({"repo":f.repo,"input_kind":"trees","base":base,"head":head}),
            )
            .await;
        let patch = f.blob(result["diff"].as_str().unwrap()).await;
        assert_eq!(patch, oracle, "{change} must match native Git");
        let text = std::str::from_utf8(&patch).expect("fixture diff is UTF-8");
        let expected: &[&str] = match change {
            "add" => &["new file mode 120000", "+./a.txt"],
            "retarget" => &[" 120000", "-./a.txt", "+./removed.txt"],
            "symlink_to_file" => &["deleted file mode 120000", "new file mode 100644"],
            "file_to_symlink" => &["deleted file mode 100644", "new file mode 120000"],
            "remove" => &["deleted file mode 120000", "-./removed.txt"],
            _ => unreachable!("fixture changes are fixed"),
        };
        for marker in expected {
            assert!(
                text.contains(marker),
                "{change}: missing {marker:?} in {text}"
            );
        }
        f.success_receipt(&result).await;
        base = head;
        base_git = head_git;
    }
}
