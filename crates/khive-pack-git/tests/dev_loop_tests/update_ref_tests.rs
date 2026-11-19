#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn update_ref_moves_an_existing_branch_and_reconcile_settles_its_receipt() {
    if crate::test_process::run_in_child() {
        return;
    }

    let f = Fixture::new(true, true).await;
    let tree = f.git_text(&["rev-parse", "HEAD^{tree}"]);
    let to = f.git_text(&[
        "commit-tree",
        &tree,
        "-p",
        &f.base,
        "-m",
        "published result",
    ]);
    let expected = f.base.clone();
    let result = f
        .call(
            "git.update_ref",
            json!({"repo":f.repo,"branch":"work","to":to,"expected":expected,"reason":"publish result"}),
        )
        .await;

    assert_eq!(result["ref"], "refs/heads/work");
    assert_eq!(result["from"], f.base);
    assert_eq!(result["to"], to);
    assert_eq!(result["fast_forward"], true);
    assert_eq!(f.git_text(&["rev-parse", "refs/heads/work"]), to);
    let receipt_id = result["receipt_id"].as_str().expect("receipt id");
    assert_eq!(
        f.receipt(receipt_id).await["inputs"]["reason"],
        "publish result"
    );
    assert_eq!(
        f.git_text(&["reflog", "-1", "--format=%gs", "refs/heads/work"]),
        format!("khive-receipt:{receipt_id}")
    );
    let mut writer = f.rt.sql().writer().await.expect("receipt writer");
    let changed = writer
        .execute(SqlStatement {
            sql: "UPDATE git_receipts SET disposition = 'unknown', finished_at = NULL WHERE id = ?1 AND namespace = 'local' AND actor = ?2".into(),
            params: vec![
                SqlValue::Text(receipt_id.into()),
                SqlValue::Text(ACTOR.into()),
            ],
            label: Some("git_update_ref_receipt_reconciliation".into()),
        })
        .await
        .expect("simulate an unsettled receipt");
    assert_eq!(changed, 1);
    drop(writer);
    assert_eq!(f.receipt(receipt_id).await["disposition"], "unknown");

    let reconciled = f.call("git.reconcile", json!({"receipt":receipt_id})).await;
    assert_eq!(reconciled["receipt"]["disposition"], "committed");
    assert_eq!(f.receipt(receipt_id).await["disposition"], "committed");
    assert_eq!(f.git_text(&["rev-parse", "refs/heads/work"]), to);
}

#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn update_ref_unchanged_head_records_marker_and_reconciles_lost_terminal_receipt() {
    if crate::test_process::run_in_child() {
        return;
    }

    let f = Fixture::new(true, true).await;
    let head = f.base.clone();
    let help = command(&f.git, &f.repo, &["reflog", "-h"], true)
        .output()
        .expect("probe Git reflog capabilities");
    if !String::from_utf8_lossy(&help.stdout).contains("git reflog write ") {
        // Ubuntu 24.04's Git lacks this newer capability. The test still
        // checks the durable refusal receipt, then skips only the marker arm.
        let error = f
            .err(
                "git.update_ref",
                json!({"repo":f.repo,"branch":"work","to":head.clone(),"expected":head}),
            )
            .await;
        assert!(error.contains("unsupported_toolchain"), "{error}");
        let receipt = f.refusal_receipt(&error).await;
        assert_eq!(
            receipt["result"]["toolchain"]["missing_capability"],
            "reflog write"
        );
        assert_eq!(
            receipt["result"]["toolchain"]["git_version"],
            f.git_text(&["--version"])
        );
        assert_eq!(f.git_text(&["rev-parse", "refs/heads/work"]), f.base);
        return;
    }
    let result = f
        .call(
            "git.update_ref",
            json!({"repo":f.repo,"branch":"work","to":head.clone(),"expected":head}),
        )
        .await;

    assert_eq!(result["from"], f.base);
    assert_eq!(result["to"], f.base);
    assert_eq!(f.git_text(&["rev-parse", "refs/heads/work"]), f.base);
    let receipt_id = result["receipt_id"].as_str().expect("receipt id");
    assert_eq!(
        f.git_text(&["reflog", "-1", "--format=%gs", "refs/heads/work"]),
        format!("khive-receipt:{receipt_id}")
    );

    // Simulate a lost terminal receipt write after the successful no-op CAS.
    let mut writer = f.rt.sql().writer().await.expect("receipt writer");
    let changed = writer
        .execute(SqlStatement {
            sql: "UPDATE git_receipts SET disposition = 'unknown', finished_at = NULL WHERE id = ?1 AND namespace = 'local' AND actor = ?2".into(),
            params: vec![
                SqlValue::Text(receipt_id.into()),
                SqlValue::Text(ACTOR.into()),
            ],
            label: Some("git_update_ref_noop_receipt_reconciliation".into()),
        })
        .await
        .expect("simulate an unsettled receipt");
    assert_eq!(changed, 1);
    drop(writer);

    let reconciled = f.call("git.reconcile", json!({"receipt":receipt_id})).await;
    assert_eq!(reconciled["receipt"]["disposition"], "committed");
    assert_eq!(f.receipt(receipt_id).await["disposition"], "committed");
    assert_eq!(f.git_text(&["rev-parse", "refs/heads/work"]), f.base);
}

#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn update_ref_rejects_non_commit_targets() {
    if crate::test_process::run_in_child() {
        return;
    }

    let f = Fixture::new(true, true).await;
    let tree = f.git_text(&["rev-parse", "HEAD^{tree}"]);
    let tagged_commit = f.git_text(&["commit-tree", &tree, "-p", &f.base, "-m", "tagged target"]);
    f.git_bytes(&[
        "tag",
        "-a",
        "release-target",
        "-m",
        "release target",
        &tagged_commit,
    ]);
    let tag = f.git_text(&["show-ref", "--hash", "refs/tags/release-target"]);
    let unknown = String::from_utf8(
        output(
            command(&f.git, &f.repo, &["hash-object", "--stdin"], true),
            Some(b"valid object bytes that are not stored\n"),
        )
        .stdout,
    )
    .expect("unknown object id UTF-8")
    .trim()
    .to_string();
    let zero = "0".repeat(40);

    for to in ["release-target", "work", &tree, &tag, &unknown, &zero] {
        let error = f
            .err(
                "git.update_ref",
                json!({"repo":f.repo,"branch":"work","to":to,"expected":f.base}),
            )
            .await;
        assert!(error.contains("invalid_params"), "{to}: {error}");
        assert_eq!(f.git_text(&["rev-parse", "refs/heads/work"]), f.base);
    }
}

#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn update_ref_refuses_a_symbolic_destination_without_moving_its_target() {
    if crate::test_process::run_in_child() {
        return;
    }

    let f = Fixture::new(true, true).await;
    let tree = f.git_text(&["rev-parse", "HEAD^{tree}"]);
    let to = f.git_text(&["commit-tree", &tree, "-p", &f.base, "-m", "candidate"]);
    f.git_bytes(&["branch", "target", &f.base]);
    f.git_bytes(&["symbolic-ref", "refs/heads/work", "refs/heads/target"]);

    let error = f
        .err(
            "git.update_ref",
            json!({"repo":f.repo,"branch":"work","to":to,"expected":f.base}),
        )
        .await;
    assert!(error.contains("ref_symbolic"), "{error}");
    assert_eq!(
        f.git_text(&["symbolic-ref", "--no-recurse", "refs/heads/work"]),
        "refs/heads/target"
    );
    assert_eq!(f.git_text(&["rev-parse", "refs/heads/target"]), f.base);
}

#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn update_ref_refuses_a_stale_expected_head_without_changing_the_branch() {
    if crate::test_process::run_in_child() {
        return;
    }

    let f = Fixture::new(true, true).await;
    let tree = f.git_text(&["rev-parse", "HEAD^{tree}"]);
    let to = f.git_text(&["commit-tree", &tree, "-p", &f.base, "-m", "candidate"]);
    let current = f.git_text(&[
        "commit-tree",
        &tree,
        "-p",
        &f.base,
        "-m",
        "concurrent update",
    ]);
    f.git_bytes(&["update-ref", "refs/heads/work", &current, &f.base]);

    let error = f
        .err(
            "git.update_ref",
            json!({"repo":f.repo,"branch":"work","to":to,"expected":f.base}),
        )
        .await;
    assert!(error.contains("expected_head_mismatch"), "{error}");
    assert_eq!(f.git_text(&["rev-parse", "refs/heads/work"]), current);
}

#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn update_ref_refuses_non_fast_forward_by_default_without_changing_the_branch() {
    if crate::test_process::run_in_child() {
        return;
    }

    let f = Fixture::new(true, true).await;
    let tree = f.git_text(&["rev-parse", "HEAD^{tree}"]);
    let to = f.git_text(&["commit-tree", &tree, "-m", "unrelated result"]);

    let error = f
        .err(
            "git.update_ref",
            json!({"repo":f.repo,"branch":"work","to":to,"expected":f.base}),
        )
        .await;
    assert!(error.contains("non_fast_forward"), "{error}");
    assert_eq!(f.git_text(&["rev-parse", "refs/heads/work"]), f.base);
}

#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn update_ref_can_allow_a_non_fast_forward_move_explicitly() {
    if crate::test_process::run_in_child() {
        return;
    }

    let f = Fixture::new(true, true).await;
    let tree = f.git_text(&["rev-parse", "HEAD^{tree}"]);
    let to = f.git_text(&["commit-tree", &tree, "-m", "unrelated result"]);
    let result = f
        .call(
            "git.update_ref",
            json!({"repo":f.repo,"branch":"work","to":to,"expected":f.base,"require_fast_forward":false}),
        )
        .await;

    assert_eq!(result["from"], f.base);
    assert_eq!(result["to"], to);
    assert_eq!(result["fast_forward"], false);
    assert_eq!(f.git_text(&["rev-parse", "refs/heads/work"]), to);
    f.success_receipt(&result).await;
}

#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn update_ref_requires_use_policy_and_checks_before_repository_gate() {
    if crate::test_process::run_in_child() {
        return;
    }

    let missing_policy = Fixture::new(true, true).await;
    missing_policy
        .call(
            "tool.policy_delete",
            json!({"actor":ACTOR,"tool":"git.update_ref"}),
        )
        .await;
    let tree = missing_policy.git_text(&["rev-parse", "HEAD^{tree}"]);
    let to = missing_policy.git_text(&[
        "commit-tree",
        &tree,
        "-p",
        &missing_policy.base,
        "-m",
        "candidate",
    ]);
    let error = missing_policy
        .err(
            "git.update_ref",
            json!({"repo":missing_policy.repo,"branch":"work","to":to,"expected":missing_policy.base}),
        )
        .await;
    assert!(error.contains("policy_denied"), "{error}");
    let receipt = missing_policy.refusal_receipt(&error).await;
    assert_eq!(receipt["policy"]["decision"], "ask");
    assert_eq!(receipt["policy"]["source"], "default");
    assert_eq!(
        missing_policy.git_text(&["rev-parse", "refs/heads/work"]),
        missing_policy.base
    );

    let missing_repository = missing_policy.dir.path().join("missing-repository");
    let error = missing_policy
        .err(
            "git.update_ref",
            json!({"repo":missing_repository,"branch":"work","to":to,"expected":missing_policy.base}),
        )
        .await;
    assert!(error.contains("policy_denied"), "{error}");
    assert_eq!(
        missing_policy.git_text(&["rev-parse", "refs/heads/work"]),
        missing_policy.base
    );

    // An empty allow-list means git writes are not configured at all, which is a different
    // refusal; the repository-absent case is an allow-listed fixture called on an unlisted path.
    let unallowlisted = Fixture::new(true, true).await;
    let outside = unallowlisted.dir.path().join("unlisted-repository");
    std::fs::create_dir(&outside).expect("unlisted directory");
    let tree = unallowlisted.git_text(&["rev-parse", "HEAD^{tree}"]);
    let to = unallowlisted.git_text(&[
        "commit-tree",
        &tree,
        "-p",
        &unallowlisted.base,
        "-m",
        "candidate",
    ]);
    let error = unallowlisted
        .err(
            "git.update_ref",
            json!({"repo":outside,"branch":"work","to":to,"expected":unallowlisted.base}),
        )
        .await;
    assert!(error.contains("repo_not_allowlisted"), "{error}");
    let receipt = unallowlisted.refusal_receipt(&error).await;
    assert_eq!(receipt["gate"]["decision"], "deny");
    assert_eq!(receipt["gate"]["source"], "git_write.allowed");
    assert_eq!(receipt["gate"]["id"], "deny:repo_not_allowlisted");
    assert_eq!(receipt["policy"]["decision"], "allow");
    assert_eq!(
        unallowlisted.git_text(&["rev-parse", "refs/heads/work"]),
        unallowlisted.base
    );

    let gate_denied = Fixture::new(true, true).await;
    gate_denied.policy("git.update_ref", "deny").await;
    let tree = gate_denied.git_text(&["rev-parse", "HEAD^{tree}"]);
    let to = gate_denied.git_text(&[
        "commit-tree",
        &tree,
        "-p",
        &gate_denied.base,
        "-m",
        "candidate",
    ]);
    let error = gate_denied
        .err(
            "git.update_ref",
            json!({"repo":gate_denied.repo,"branch":"work","to":to,"expected":gate_denied.base}),
        )
        .await;
    assert!(error.contains("policy_denied"), "{error}");
    assert_eq!(
        gate_denied.git_text(&["rev-parse", "refs/heads/work"]),
        gate_denied.base
    );
}
