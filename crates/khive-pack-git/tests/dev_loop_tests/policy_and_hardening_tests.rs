#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn arm15_overrides_and_unmapped_actor_refuse_before_resolver() {
    if crate::test_process::run_in_child() {
        return;
    }

    for mapped in [true, false] {
        let f = Fixture::new(true, mapped).await;
        f.policy("git.commit", "allow").await;
        let manifest = f.tree(&[("a.txt", b"new\n", 644)]).await;
        let refs = f.git_bytes(&["show-ref"]);
        for key in ["author", "actor", "credential"] {
            let mut params = f.commit_params(&manifest);
            params[key] = json!(TOKEN);
            let error = f.err("git.commit", params).await;
            let row = f.refusal_receipt(&error).await;
            assert_eq!(row["reason"], "invalid_params");
            assert!(!error.contains(TOKEN));
            assert!(!row.to_string().contains(TOKEN));
        }
        if !mapped {
            let error = f.err("git.commit", f.commit_params(&manifest)).await;
            let row = f.refusal_receipt(&error).await;
            assert_eq!(row["reason"], "actor_unmapped");
            // The reason alone does not say which table was read, and this verb reads
            // only the actors one (ADR-182 Amendment 10 item 2).
            assert_eq!(
                row["result"]["refusal"],
                json!({"table":"git_write.actors","key":ACTOR,"cause":"absent"})
            );
        }
        assert_eq!(f.resolver_count(), 0);
        assert_eq!(f.git_bytes(&["show-ref"]), refs);
    }
}

#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn arm16_hostile_hooks_filters_and_config_never_execute() {
    if crate::test_process::run_in_child() {
        return;
    }

    let f = Fixture::new(true, true).await;
    f.policy("git.commit", "allow").await;
    let hooks = f.dir.path().join("hooks");
    std::fs::create_dir(&hooks).expect("hooks directory");
    let hook_marker = f.dir.path().join("hook.marker");
    let filter_marker = f.dir.path().join("filter.marker");
    let config_marker = f.dir.path().join("config.marker");
    executable(
        &hooks.join("reference-transaction"),
        &format!(
            "#!/bin/sh\nprintf hook >> {}\nexit 0\n",
            quoted(&hook_marker)
        ),
    );
    let filter = f.dir.path().join("filter");
    executable(
        &filter,
        &format!(
            "#!/bin/sh\nprintf filter >> {}\nprintf FILTERED\n",
            quoted(&filter_marker)
        ),
    );
    let hostile = f.dir.path().join("hostile-program");
    executable(
        &hostile,
        &format!(
            "#!/bin/sh\nprintf config >> {}\nexit 1\n",
            quoted(&config_marker)
        ),
    );
    f.git_bytes(&["config", "core.hooksPath", hooks.to_str().unwrap()]);
    f.git_bytes(&["config", "filter.hostile.clean", filter.to_str().unwrap()]);
    f.git_bytes(&["config", "filter.hostile.smudge", filter.to_str().unwrap()]);
    f.git_bytes(&["config", "core.fsmonitor", hostile.to_str().unwrap()]);
    f.git_bytes(&["config", "commit.gpgsign", "true"]);
    f.git_bytes(&["config", "gpg.program", hostile.to_str().unwrap()]);
    std::fs::write(f.repo.join(".gitattributes"), b"a.txt filter=hostile\n")
        .expect("hostile attributes");

    output(
        command(
            &f.git,
            &f.repo,
            &["update-ref", "refs/heads/hook-control", &f.base, ZERO],
            false,
        ),
        None,
    );
    assert!(
        hook_marker.exists(),
        "reference-transaction positive control is inert"
    );
    f.git_bytes(&["update-ref", "-d", "refs/heads/hook-control"]);
    std::fs::remove_file(&hook_marker).expect("clear positive-control hook marker");
    output(
        command(
            &f.git,
            &f.repo,
            &["hash-object", "--path=a.txt", "--stdin"],
            true,
        ),
        Some(b"filter input\n"),
    );
    assert!(
        filter_marker.exists(),
        "clean-filter positive control is inert"
    );
    std::fs::remove_file(&filter_marker).expect("clear filter marker");
    let _env = EnvGuard::set(&[
        ("GIT_CONFIG_COUNT", "1".into()),
        ("GIT_CONFIG_KEY_0", "core.hooksPath".into()),
        ("GIT_CONFIG_VALUE_0", hooks.as_os_str().to_owned()),
        ("GIT_AUTHOR_NAME", "Injected Author".into()),
    ]);
    let raw = b"raw\r\n\0\xffbytes\n";
    let manifest = f.tree(&[("a.txt", raw, 644)]).await;
    let result = f.call("git.commit", f.commit_params(&manifest)).await;
    assert!(!hook_marker.exists(), "HOOK_CONTROL_MUST_NOT_RUN");
    assert!(!filter_marker.exists(), "clean or smudge filter ran");
    assert!(!config_marker.exists(), "fsmonitor or signer ran");
    let sha = result["sha"].as_str().unwrap();
    assert_eq!(
        f.git_bytes(&["cat-file", "blob", &format!("{sha}:a.txt")]),
        raw
    );
    assert_eq!(
        f.git_text(&["show", "-s", "--format=%an", sha]),
        "Mapped Author"
    );
    f.success_receipt(&result).await;
}

#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn arm26_receipts_bind_real_policy_ids_and_gate_denial_has_null_policy() {
    if crate::test_process::run_in_child() {
        return;
    }

    for allowlisted in [false, true] {
        let f = Fixture::new(allowlisted, false).await;
        let policy_id = f.policy("git.branch", "allow").await;
        let params = json!({"repo":f.repo,"name":"new","from":f.base});
        if allowlisted {
            let result = f.call("git.branch", params).await;
            let row = f.success_receipt(&result).await;
            assert_eq!(
                row["gate"],
                json!({"decision":"allow","source":"git_write.allowed","id":0})
            );
            assert_eq!(
                row["policy"],
                json!({"decision":"allow","source":"policy","id":policy_id})
            );
        } else {
            let error = f.err("git.branch", params).await;
            let row = f.refusal_receipt(&error).await;
            assert_eq!(row["gate"]["decision"], "deny");
            assert!(row["policy"].is_null());
        }
    }
}
