#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn arm28_symbolic_branch_refuses_both_moves_and_plain_ref_controls_proceed() {
    if crate::test_process::run_in_child() {
        return;
    }

    let f = Fixture::new(true, true).await;
    f.policy("git.branch", "allow").await;
    f.policy("git.commit", "allow").await;
    let manifest = f.tree(&[("a.txt", b"plain ref commit\n", 644)]).await;
    f.git_bytes(&["symbolic-ref", "refs/heads/symbolic", "refs/heads/work"]);
    let refs = f.git_bytes(&["show-ref"]);
    let alias = std::fs::read(f.repo.join(".git/refs/heads/symbolic")).expect("symbolic ref bytes");
    let branch_params = json!({"repo":f.repo,"name":"symbolic","from":f.base,"expected":f.base});
    let mut commit_params = f.commit_params(&manifest);
    commit_params["branch"] = json!("symbolic");
    for (verb, params) in [
        ("git.branch", branch_params.clone()),
        ("git.commit", commit_params.clone()),
    ] {
        let error = f.err(verb, params).await;
        assert_eq!(f.refusal_receipt(&error).await["reason"], "ref_symbolic");
        assert_eq!(f.git_bytes(&["show-ref"]), refs);
        assert_eq!(
            std::fs::read(f.repo.join(".git/refs/heads/symbolic")).expect("alias after refusal"),
            alias
        );
    }
    assert_eq!(f.resolver_count(), 0);

    // Removing only the alias makes the same create call legal; its target stays put.
    f.git_bytes(&["symbolic-ref", "--delete", "refs/heads/symbolic"]);
    assert_eq!(f.git_text(&["rev-parse", "work"]), f.base);
    let branch = f.call("git.branch", branch_params).await;
    f.success_receipt(&branch).await;
    let plain = command(
        &f.git,
        &f.repo,
        &["symbolic-ref", "--quiet", "refs/heads/symbolic"],
        true,
    )
    .output()
    .expect("inspect plain ref");
    assert_eq!(
        plain.status.code(),
        Some(1),
        "positive-control ref is still symbolic"
    );
    let result = f.call("git.commit", commit_params).await;
    f.success_receipt(&result).await;
    assert_eq!(
        f.git_text(&["rev-parse", "symbolic"]),
        result["sha"].as_str().unwrap()
    );
    assert_eq!(
        f.git_text(&["rev-parse", "work"]),
        f.base,
        "symbolic target must never move"
    );
    assert_eq!(f.resolver_count(), 1);
}

#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn arm29_reconcile_requires_matching_receipt_marker_and_new_sha() {
    if crate::test_process::run_in_child() {
        return;
    }

    let f = Fixture::new(true, true).await;
    let policy_id = f.policy("git.commit", "allow").await;
    let native_tree = f.git_text(&["rev-parse", "HEAD^{tree}"]);
    let intended = f.git_text(&["commit-tree", &native_tree, "-p", &f.base, "-m", "intended"]);
    let other = f.git_text(&[
        "commit-tree",
        &native_tree,
        "-p",
        &f.base,
        "-m",
        "different move",
    ]);
    let descendant = f.git_text(&[
        "commit-tree",
        &native_tree,
        "-p",
        &intended,
        "-m",
        "advanced descendant",
    ]);
    for evidence in [
        "rival",
        "wrong-sha",
        "pruned",
        "rewound",
        "current",
        "matching",
    ] {
        let id = uuid::Uuid::new_v4().to_string();
        let branch = format!("reconcile-{evidence}");
        let reference = format!("refs/heads/{branch}");
        let marker = format!("khive-receipt:{id}");
        f.git_bytes(&[
            "update-ref",
            "--no-deref",
            "--create-reflog",
            "-m",
            "baseline",
            &reference,
            &f.base,
            ZERO,
        ]);
        f.seed_unknown_receipt(&id, &branch, &intended, &policy_id)
            .await;
        match evidence {
            "rival" => {
                f.git_bytes(&[
                    "update-ref",
                    "--no-deref",
                    "--create-reflog",
                    "-m",
                    "rival-installed",
                    &reference,
                    &intended,
                    &f.base,
                ]);
            }
            "wrong-sha" => {
                f.git_bytes(&[
                    "update-ref",
                    "--no-deref",
                    "--create-reflog",
                    "-m",
                    &marker,
                    &reference,
                    &other,
                    &f.base,
                ]);
                f.git_bytes(&[
                    "update-ref",
                    "--no-deref",
                    "--create-reflog",
                    "-m",
                    "rival-installed",
                    &reference,
                    &intended,
                    &other,
                ]);
            }
            "pruned" => {
                f.git_bytes(&[
                    "update-ref",
                    "--no-deref",
                    "--create-reflog",
                    "-m",
                    &marker,
                    &reference,
                    &intended,
                    &f.base,
                ]);
                f.git_bytes(&["reflog", "expire", "--expire=all", &reference]);
            }
            "rewound" => {
                f.git_bytes(&[
                    "update-ref",
                    "--no-deref",
                    "--create-reflog",
                    "-m",
                    &marker,
                    &reference,
                    &intended,
                    &f.base,
                ]);
                f.git_bytes(&[
                    "update-ref",
                    "--no-deref",
                    "--create-reflog",
                    "-m",
                    "rewound",
                    &reference,
                    &f.base,
                    &intended,
                ]);
            }
            "current" => {
                f.git_bytes(&[
                    "update-ref",
                    "--no-deref",
                    "--create-reflog",
                    "-m",
                    &marker,
                    &reference,
                    &intended,
                    &f.base,
                ]);
            }
            "matching" => {
                f.git_bytes(&[
                    "update-ref",
                    "--no-deref",
                    "--create-reflog",
                    "-m",
                    &marker,
                    &reference,
                    &intended,
                    &f.base,
                ]);
                f.git_bytes(&[
                    "update-ref",
                    "--no-deref",
                    "--create-reflog",
                    "-m",
                    "later-descendant",
                    &reference,
                    &descendant,
                    &intended,
                ]);
            }
            _ => unreachable!(),
        }
        let log_before = f.git_bytes(&["reflog", "show", "--format=%H %gs", &reference]);
        let witness = format!("{intended} {marker}\n");
        assert_eq!(
            String::from_utf8_lossy(&log_before).contains(&witness),
            matches!(evidence, "rewound" | "current" | "matching")
        );
        if matches!(evidence, "rival" | "wrong-sha" | "pruned") {
            assert_eq!(
                f.git_text(&["rev-parse", &reference]),
                intended,
                "all negative controls have the intended current SHA"
            );
        }
        let refs_before = f.git_bytes(&["show-ref"]);
        let result = f.call("git.reconcile", json!({"receipt":id})).await;
        let expected = if matches!(evidence, "current" | "matching") {
            "committed"
        } else {
            "unknown"
        };
        assert_eq!(
            result["receipt"]["disposition"], expected,
            "reconcile evidence case {evidence}"
        );
        assert_eq!(f.receipt(&id).await["disposition"], expected);
        assert_eq!(
            f.git_bytes(&["show-ref"]),
            refs_before,
            "reconcile must never retry the ref move"
        );
        assert_eq!(
            f.git_bytes(&["reflog", "show", "--format=%H %gs", &reference]),
            log_before
        );
        f.success_receipt(&result).await;
    }
    assert_eq!(
        f.resolver_count(),
        0,
        "read-only reconciliation must not resolve a credential"
    );
}

#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn arm30_tool_pack_absence_refuses_tree_commit_but_preserves_legacy_paths() {
    if crate::test_process::run_in_child() {
        return;
    }

    let f = Fixture::new_with_tool_pack(true, true, false).await;
    assert!(f.registry.has_verb("git.commit"));
    assert!(!f.registry.has_verb("tool.check"));
    assert!(!f.registry.has_verb("exec.tree_put"));
    let mut reader = f.rt.sql().reader().await.expect("schema reader");
    let tables = reader.query_all(SqlStatement {
        sql:"SELECT name FROM sqlite_master WHERE type = 'table' AND name IN ('tool_policy', 'tool_grants')".into(),
        params:vec![], label:Some("git_test_absent_tool_schema".into()),
    }).await.expect("inspect real schema");
    drop(reader);
    assert!(
        tables.is_empty(),
        "missing-pack control accidentally installed tool tables"
    );
    let blob = tree::blob_store(&f.rt)
        .expect("blob store")
        .put(b"tree candidate\n".to_vec())
        .await
        .expect("manifest content");
    let manifest = tree::store(
        &f.rt,
        &[tree::TreeEntry {
            path: "a.txt".into(),
            content_ref: blob.as_str().into(),
            mode: 644,
        }],
    )
    .await
    .expect("existing tree manifest without exec dispatch");
    let refs = f.git_bytes(&["show-ref"]);
    let error = f.err("git.commit", f.commit_params(&manifest)).await;
    let row = f.stored_refusal_receipt(&error).await;
    assert_eq!(row["reason"], "policy_unavailable");
    assert_eq!(
        row["gate"],
        json!({"decision":"allow","source":"git_write.allowed","id":0})
    );
    assert_eq!(
        row["policy"],
        json!({"decision":"deny","source":"policy_unavailable","id":null})
    );
    assert_eq!(f.git_bytes(&["show-ref"]), refs);
    assert_eq!(f.resolver_count(), 0);

    f.git_bytes(&["config", "user.name", "Legacy Fixture"]);
    f.git_bytes(&["config", "user.email", "legacy@example.invalid"]);
    f.git_bytes(&["config", "commit.gpgsign", "false"]);
    std::fs::write(f.repo.join("a.txt"), b"legacy paths content\n").expect("legacy content");
    let _identity = EnvGuard::remove(&[
        "GIT_AUTHOR_NAME",
        "GIT_AUTHOR_EMAIL",
        "GIT_COMMITTER_NAME",
        "GIT_COMMITTER_EMAIL",
        "GIT_AUTHOR_DATE",
        "GIT_COMMITTER_DATE",
        "GIT_CONFIG_PARAMETERS",
        "GIT_DIR",
        "GIT_COMMON_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_EXEC_PATH",
    ]);
    let _config = EnvGuard::set(&[
        ("GIT_CONFIG_COUNT", "0".into()),
        ("GIT_CONFIG_NOSYSTEM", "1".into()),
        ("GIT_CONFIG_SYSTEM", "/dev/null".into()),
        ("GIT_CONFIG_GLOBAL", "/dev/null".into()),
    ]);
    let legacy = f
        .call(
            "git.commit",
            json!({"repo":f.repo,"paths":["a.txt"],"message":"legacy paths commit","branch":"work","expected_head":f.base}),
        )
        .await;
    let sha = legacy["sha"].as_str().expect("legacy commit SHA");
    assert_eq!(f.git_text(&["rev-parse", "work"]), sha);
    assert_eq!(
        f.git_bytes(&["cat-file", "blob", &format!("{sha}:a.txt")]),
        b"legacy paths content\n"
    );
    assert_eq!(
        f.git_text(&["show", "-s", "--format=%an <%ae>", sha]),
        "Legacy Fixture <legacy@example.invalid>"
    );
    assert_eq!(f.resolver_count(), 0);
}

#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn arm31_reflog_append_without_ref_install_stays_unknown_until_descendant_installed() {
    if crate::test_process::run_in_child() {
        return;
    }

    let f = Fixture::new(true, true).await;
    let policy_id = f.policy("git.commit", "allow").await;
    let native_tree = f.git_text(&["rev-parse", "HEAD^{tree}"]);
    let candidate = f.git_text(&[
        "commit-tree",
        &native_tree,
        "-p",
        &f.base,
        "-m",
        "candidate",
    ]);
    let descendant = f.git_text(&[
        "commit-tree",
        &native_tree,
        "-p",
        &candidate,
        "-m",
        "descendant",
    ]);
    let branch = "reflog-before-install";
    let reference = format!("refs/heads/{branch}");
    f.git_bytes(&[
        "update-ref",
        "--no-deref",
        "--create-reflog",
        "-m",
        "baseline",
        &reference,
        &f.base,
        ZERO,
    ]);
    let id = uuid::Uuid::new_v4().to_string();
    f.seed_unknown_receipt(&id, branch, &candidate, &policy_id)
        .await;
    let marker = format!("khive-receipt:{id}");
    let log_path = f.repo.join(".git/logs").join(&reference);
    let mut log = std::fs::OpenOptions::new()
        .append(true)
        .open(&log_path)
        .expect("real reflog");
    let entry = format!(
        "{} {} Fixture Author <fixture@example.invalid> {} +0000\t{}\n",
        f.base,
        candidate,
        chrono::Utc::now().timestamp(),
        marker
    );
    log.write_all(entry.as_bytes())
        .expect("append pre-install reflog witness");
    drop(log);
    assert_eq!(
        f.git_text(&["rev-parse", &reference]),
        f.base,
        "raw reflog append must not move a ref"
    );
    let witness = format!("{candidate} {marker}\n");
    assert!(String::from_utf8_lossy(&f.git_bytes(&[
        "reflog",
        "show",
        "--format=%H %gs",
        &reference
    ]))
    .contains(&witness));
    let refs = f.git_bytes(&["show-ref"]);
    let first = f.call("git.reconcile", json!({"receipt":id})).await;
    assert_eq!(
        first["receipt"]["disposition"], "unknown",
        "marker alone cannot prove the ref installed"
    );
    assert_eq!(f.receipt(&id).await["disposition"], "unknown");
    assert_eq!(f.git_bytes(&["show-ref"]), refs);

    f.git_bytes(&[
        "update-ref",
        "--no-deref",
        "--create-reflog",
        "-m",
        "descendant-installed",
        &reference,
        &descendant,
        &f.base,
    ]);
    f.git_bytes(&["merge-base", "--is-ancestor", &candidate, &descendant]);
    let refs = f.git_bytes(&["show-ref"]);
    let second = f.call("git.reconcile", json!({"receipt":id})).await;
    assert_eq!(second["receipt"]["disposition"], "committed");
    assert_eq!(f.receipt(&id).await["disposition"], "committed");
    assert_eq!(f.git_bytes(&["show-ref"]), refs);
    assert_eq!(f.resolver_count(), 0);
}
