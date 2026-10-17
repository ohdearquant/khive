#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn amendment12_arm5_program_is_unknown_on_every_repository_verb() {
    if crate::test_process::run_in_child() {
        return;
    }

    let f = Fixture::new(true, true).await;
    let manifest = f.tree(&[("a.txt", b"caller program control\n", 644)]).await;
    let refs = f.git_bytes(&["show-ref"]);
    let index = std::fs::read(f.repo.join(".git/index")).expect("index before refused calls");
    let handlers: Vec<_> = f
        .registry
        .pack_verbs("git")
        .expect("registered Git handlers")
        .iter()
        .filter(|handler| handler.params.iter().any(|param| param.name == "repo"))
        .collect();
    assert!(!handlers.is_empty());
    for handler in handlers {
        let verb = handler.name;
        let cases = match verb {
            "git.init" => vec![json!({"repo":f.blank,"branch":"main"})],
            "git.checkout" => vec![json!({"repo":f.repo,"ref":f.base})],
            "git.diff" => {
                vec![json!({"repo":f.repo,"input_kind":"commits","base":f.base,"head":f.base})]
            }
            "git.branch" => {
                vec![
                    json!({"repo":f.repo,"name":"program-control","from":f.base,"expected":f.base}),
                ]
            }
            "git.update_ref" => {
                vec![json!({"repo":f.repo,"branch":"work","to":f.base,"expected":f.base})]
            }
            "git.commit" => vec![
                f.commit_params(&manifest),
                json!({"repo":f.repo,"paths":["a.txt"],"message":"legacy program control"}),
            ],
            "git.push" => {
                vec![
                    json!({"repo":f.repo,"branch":"work","expected_local":f.base,"expected_remote":null}),
                ]
            }
            "git.pr_open" => {
                vec![
                    json!({"repo":f.repo,"head":"work","base":"main","title":"Program control","body":"","expected_head":f.base}),
                ]
            }
            "git.pr_review" => {
                vec![
                    json!({"repo":f.repo,"number":1,"verdict":"comment","body":"Program control","expected_head":f.base}),
                ]
            }
            "git.pr_merge" => {
                vec![
                    json!({"repo":f.repo,"number":1,"method":"squash","subject":"Program control","body":"","expected_head":f.base}),
                ]
            }
            "git.receipts" | "git.gates" | "git.status" | "git.log" => {
                vec![json!({"repo":f.repo})]
            }
            _ => panic!("repository-taking Git verb needs an arm 5 payload: {verb}"),
        };
        // The fixture already allows some of these verbs, and rewriting an identical policy is
        // refused as `policy_unchanged`.
        if !FIXTURE_ALLOWED_VERBS.contains(&verb) {
            f.policy(verb, "allow").await;
        }
        let help = f.registry.describe_verb(verb).expect("Git verb schema");
        let schema = &help["input_schema"];
        assert_eq!(schema["additionalProperties"], false, "{verb}");
        let properties = schema["properties"].as_object().expect("schema properties");
        assert!(!properties.contains_key("program"), "{verb}");
        for mut params in cases {
            for key in params.as_object().expect("object payload").keys() {
                assert!(
                    properties.contains_key(key),
                    "{verb}: undeclared fixture field {key}"
                );
            }
            for required in schema["required"].as_array().expect("required fields") {
                let required = required.as_str().expect("required field name");
                assert!(
                    params.get(required).is_some(),
                    "{verb}: missing fixture field {required}"
                );
            }
            params["program"] = json!(f.git);
            let error = f.err(verb, params.clone()).await;
            assert!(
                error.contains("unknown field `program`"),
                "{verb} {params}: {error}"
            );
        }
    }
    assert_eq!(f.git_bytes(&["show-ref"]), refs);
    assert_eq!(std::fs::read(f.repo.join(".git/index")).unwrap(), index);
    assert!(!f.blank.join(".git").exists());
    assert_eq!(f.resolver_count(), 0);
}

#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn arm13_existing_branch_refuses_without_repository_changes() {
    if crate::test_process::run_in_child() {
        return;
    }

    let f = Fixture::new(true, false).await;
    f.policy("git.branch", "allow").await;
    let refs = f.git_bytes(&["show-ref"]);
    let objects = f.git_bytes(&["count-objects", "-v"]);
    let error = f
        .err(
            "git.branch",
            json!({"repo":f.repo,"name":"work","from":f.base}),
        )
        .await;
    f.refusal_receipt(&error).await;
    assert_eq!(f.git_bytes(&["show-ref"]), refs);
    assert_eq!(f.git_bytes(&["count-objects", "-v"]), objects);
    assert_eq!(f.resolver_count(), 0);
    let result = f
        .call(
            "git.branch",
            json!({"repo":f.repo,"name":"new","expected":f.base}),
        )
        .await;
    let row = f.success_receipt(&result).await;
    assert_eq!(f.git_text(&["rev-parse", "new"]), f.base);
    assert!(row["credential"].is_null());
    assert_eq!(f.resolver_count(), 0);
}

#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn arm14_moved_parent_refuses_before_ref_move() {
    if crate::test_process::run_in_child() {
        return;
    }

    let f = Fixture::new(true, true).await;
    f.policy("git.commit", "allow").await;
    let manifest = f.tree(&[("a.txt", b"new\n", 644)]).await;
    f.git_bytes(&["commit", "--allow-empty", "-q", "-m", "rival"]);
    let refs = f.git_bytes(&["show-ref"]);
    let error = f.err("git.commit", f.commit_params(&manifest)).await;
    assert_eq!(
        f.refusal_receipt(&error).await["reason"],
        "expected_head_mismatch"
    );
    assert_eq!(f.git_bytes(&["show-ref"]), refs);
    assert_eq!(f.resolver_count(), 0);
}

#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn arm14_ref_move_race_preserves_rival_with_atomic_compare() {
    if crate::test_process::run_in_child() {
        return;
    }

    let f = Fixture::new(true, true).await;
    f.policy("git.commit", "allow").await;
    let manifest = f.tree(&[("a.txt", b"ours\n", 644)]).await;
    let git_tree = f.git_text(&["rev-parse", "HEAD^{tree}"]);
    let rival = f.git_text(&["commit-tree", &git_tree, "-p", &f.base, "-m", "rival"]);
    let bin = f.dir.path().join("interposer");
    std::fs::create_dir(&bin).expect("interposer directory");
    let marker = f.dir.path().join("raced");
    executable(&bin.join("git"), &format!(
        "#!/bin/sh\nis_update=0\nfor argument do\n  if [ \"$argument\" = update-ref ]; then is_update=1; fi\n  if [ \"$is_update\" = 1 ] && [ \"$argument\" = refs/heads/work ] && [ ! -e {marker} ]; then\n    printf raced > {marker}\n    {git} -c core.hooksPath=/dev/null -C {repo} update-ref refs/heads/work {rival} {base} || exit 91\n  fi\ndone\nexec {git} \"$@\"\n",
        marker=quoted(&marker), git=quoted(&f.git), repo=quoted(&f.repo), rival=rival, base=f.base,
    ));
    let mut paths = vec![bin];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").expect("PATH"),
    ));
    let _env = EnvGuard::set(&[(
        "PATH",
        std::env::join_paths(paths).expect("interposer PATH"),
    )]);
    let outcome = f
        .registry
        .dispatch("git.commit", f.commit_params(&manifest))
        .await;
    assert!(
        marker.exists(),
        "CAS race control never reached native update-ref"
    );
    assert_eq!(
        f.git_text(&["rev-parse", "work"]),
        rival,
        "CAS_CONTROL_RIVAL_MUST_SURVIVE"
    );
    let error = outcome
        .expect_err("CAS_CONTROL_COMPARE_MUST_REFUSE")
        .to_string();
    f.refusal_receipt(&error).await;
}
