#[tokio::test]
async fn remote_lost_ack_without_marker_stays_unknown_and_never_retries() {
    let f = Fixture::new(true, None).await;
    f.remote.state.lock().unwrap().lost_ack = true;
    assert!(f.call(&f.actor, "git.push", f.push()).await.is_err());
    let receipt = f.last(&f.actor).await;
    assert_eq!(receipt.disposition, Disposition::Unknown);
    assert_eq!(remote_head(&f.remote.bare), Some(f.head.clone()));
    let result = f
        .call(&f.actor, "git.reconcile", json!({"receipt":receipt.id}))
        .await
        .unwrap();
    assert_eq!(result["receipt"]["disposition"], "unknown");
    assert_eq!(f.remote.state.lock().unwrap().push_calls, 1);
    f.refusal(&f.actor, "git.push", f.push(), "expected_remote_mismatch")
        .await;
}

#[tokio::test]
async fn pr_open_and_review_reconcile_return_unknown_without_platform_evidence() {
    let f = Fixture::new(true, None).await;
    for (actor, verb, inputs) in [
        (
            &f.actor,
            "git.pr_open",
            json!({"head":"work","base":"main","expected_head":f.head}),
        ),
        (
            &f.reviewer,
            "git.pr_review",
            json!({"number":1,"verdict":"approve","expected_head":f.head}),
        ),
    ] {
        let mut prior = Receipt::new(
            "local",
            actor,
            verb,
            f.repo.to_str().unwrap(),
            inputs,
            json!({"decision":"allow","source":"git_write.allowed","id":0}),
            Value::Null,
        );
        prior.result = json!({"slug":SLUG,"remote":REMOTE});
        receipts::insert(&f.rt, &prior).await.unwrap();
        let calls = f.remote.state.lock().unwrap().calls.clone();
        let writes = f.remote.writes();

        let result = f
            .call(actor, "git.reconcile", json!({"receipt":prior.id}))
            .await
            .unwrap();
        assert_eq!(result["receipt"]["id"], prior.id);
        assert_eq!(result["receipt"]["disposition"], "unknown");
        assert_eq!(result["receipt"]["result"], prior.result);
        let stored = receipts::load_owned(&f.rt, "local", actor, &prior.id)
            .await
            .unwrap();
        assert_eq!(stored.disposition, Disposition::Unknown);
        assert_eq!(stored.finished_at, None);
        assert_eq!(stored.result, prior.result);
        assert_eq!(f.remote.state.lock().unwrap().calls, calls);
        assert_eq!(f.remote.writes(), writes);
    }
    assert!(
        !f.dir.path().join("reads").exists(),
        "reconcile must not resolve either actor's platform credential"
    );
}

/// Every push case in this crate reaches `push_marker_support`, so on a host whose `git` does not
/// advertise `git reflog write ` each of them fails on the `unsupported_toolchain` refusal and the
/// run reads as nineteen defects rather than one fact about the host. This case states the fact
/// once. It holds `ENV_MUTEX` because the case below installs a git that answers `reflog -h` for
/// every caller while it runs.
#[tokio::test]
async fn push_cases_require_a_git_that_advertises_reflog_write() {
    let _env_guard = crate::cache::ENV_MUTEX.lock().await;
    let dir = tempfile::tempdir().expect("temp dir");
    let repo = dir.path().join("probe");
    let init = Command::new("git")
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .args(["init", "--quiet"])
        .arg(&repo)
        .output()
        .expect("spawn git init");
    assert!(
        init.status.success(),
        "git init: {}",
        String::from_utf8_lossy(&init.stderr)
    );
    let (version, supported) = crate::local_git::push_marker_support(Path::new("git"), &repo)
        .await
        .expect("capability probe");
    assert!(
        supported,
        "the git on PATH is {version}, which does not advertise `git reflog write `. \
         git.push writes its receipt marker with that subcommand, so every push case in \
         remote_tests and local_remote_tests refuses unsupported_toolchain on this host, and \
         those failures are about the host and not about the pack. The capability first shipped \
         in Git 2.51; the pack reads the capability and never the version number. Put a git that \
         advertises it first on PATH."
    );
}

#[tokio::test]
async fn remote_unsupported_git_is_receipted_before_credentials_or_network() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("old-git");
    std::fs::create_dir(&bin).unwrap();
    let wrapper = bin.join("git");
    let f = Fixture::with_program(true, None, &[], || {
        let git_path = Command::new("/usr/bin/which").arg("git").output().unwrap();
        assert!(git_path.status.success());
        let git_path = PathBuf::from(String::from_utf8(git_path.stdout).unwrap().trim());
        std::fs::write(&wrapper, format!("#!/bin/sh\ncase \"$*\" in\n*' --version') echo 'git version 2.40.0'; exit 0;;\n*' reflog -h') echo 'usage: git reflog show'; exit 129;;\nesac\nexec {} \"$@\"\n", quote(&git_path))).unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
        Some(wrapper.clone())
    }).await;
    f.refusal(&f.actor, "git.push", f.push(), "unsupported_toolchain")
        .await;
    let receipt = f.last(&f.actor).await;
    assert_eq!(
        receipt.result["toolchain"]["git_version"],
        "git version 2.40.0"
    );
    assert_eq!(
        receipt.result["toolchain"]["missing_capability"],
        "reflog write"
    );
    assert_eq!(receipt.result["toolchain"]["git_program"], json!(wrapper));
    assert!(receipt.credential.is_null());
    assert!(f.remote.state.lock().unwrap().calls.is_empty());
    assert_eq!(remote_head(&f.remote.bare), Some(f.base.clone()));
}

#[tokio::test]
async fn remote_malformed_merge_reply_stays_unknown_until_readback() {
    let f = Fixture::new(true, None).await;
    let n = f.open().await;
    f.review(&f.reviewer, n).await.unwrap();
    f.remote.state.lock().unwrap().malformed_merge_reply = true;
    assert!(f.call(&f.actor, "git.pr_merge", f.merge(n)).await.is_err());
    let receipt = f.last(&f.actor).await;
    assert_eq!(receipt.disposition, Disposition::Unknown);
    assert_eq!(f.remote.pr(n)["merged"], true);
    let writes = f.remote.writes();
    let result = f
        .call(&f.actor, "git.reconcile", json!({"receipt":receipt.id}))
        .await
        .unwrap();
    assert_eq!(result["receipt"]["disposition"], "committed");
    assert_eq!(f.remote.writes(), writes);
}

#[cfg(feature = "contract-faults")]
#[tokio::test]
async fn remote_post_effect_faults_reconcile_without_repeating_native_write() {
    for verb in ["git.push", "git.pr_merge"] {
        for point in ["reply-lost-after-effect", "audit-fails-after-effect"] {
            let fault = format!("{verb}:{point}");
            let f = Fixture::new(true, Some(&fault)).await;
            let mut params = if verb == "git.push" {
                f.push()
            } else {
                let n = f.open().await;
                f.review(&f.reviewer, n).await.unwrap();
                f.merge(n)
            };
            // Accepted hexadecimal spelling must not obstruct later reconciliation.
            let compare = if verb == "git.push" {
                "expected_local"
            } else {
                "expected_head"
            };
            params[compare] = json!(f.head.to_ascii_uppercase());
            assert!(f.call(&f.actor, verb, params.clone()).await.is_err());
            let receipt = f.last(&f.actor).await;
            assert_eq!(
                receipt.disposition,
                if point == "audit-fails-after-effect" {
                    Disposition::Committed
                } else {
                    Disposition::Unknown
                }
            );
            let writes = f.remote.writes();
            let result = f
                .call(&f.actor, "git.reconcile", json!({"receipt":receipt.id}))
                .await
                .unwrap();
            assert_eq!(
                result["receipt"]["disposition"], "committed",
                "{verb}:{point}: {result}"
            );
            assert_eq!(f.remote.writes(), writes);
            assert!(f.call(&f.actor, verb, params).await.is_err());
            assert_eq!(f.remote.writes(), writes);
        }
    }
}
