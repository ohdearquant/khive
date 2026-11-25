// `git.branch_delete` against the recording platform. The fixture's repository has `work`
// checked out, so a case that means to delete it first detaches HEAD.

fn delete_params(f: &Fixture) -> Value {
    json!({"name":"work","expected":f.head})
}

fn delete_detach(f: &Fixture) {
    git(&f.repo, &["checkout", "-q", "--detach"]);
}

async fn delete_ready(f: &Fixture) {
    delete_detach(f);
    f.policy(&f.actor, "git.branch_delete", "allow").await;
}

fn delete_calls(f: &Fixture) -> Vec<Value> {
    f.remote.state.lock().unwrap().calls.clone()
}

/// Where `work` points in the fixture's repository, or `None` when it is absent.
fn work_tip(f: &Fixture) -> Option<String> {
    let out = Command::new("git")
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .arg("-C")
        .arg(&f.repo)
        .args(["rev-parse", "--verify", "--quiet", "refs/heads/work"])
        .output()
        .unwrap();
    out.status
        .success()
        .then(|| String::from_utf8(out.stdout).unwrap().trim().into())
}

/// The hook that runs between the last check and the delete, removed again when the test ends.
/// The fixture holds the environment lock for the test's whole length.
struct DeleteRace;

impl DeleteRace {
    fn set(hook: crate::remote_handlers::branch_delete::RaceHook) -> Self {
        crate::remote_handlers::branch_delete::set_race_hook(Some(hook));
        Self
    }
}

impl Drop for DeleteRace {
    fn drop(&mut self) {
        crate::remote_handlers::branch_delete::set_race_hook(None);
    }
}

#[tokio::test]
async fn remote_branch_delete_removes_the_branch_after_two_reads_and_writes_nothing_else() {
    let f = Fixture::new(true, None).await;
    delete_ready(&f).await;

    let reply = f
        .call(&f.actor, "git.branch_delete", delete_params(&f))
        .await
        .unwrap();

    assert_eq!(reply["ref"], "refs/heads/work");
    assert_eq!(reply["from"], f.head);
    assert_eq!(reply["repo"], f.repo.display().to_string());
    assert_eq!(work_tip(&f), None);
    // Only the name went: the commit is still in the repository.
    assert_eq!(git(&f.repo, &["cat-file", "-t", &f.head]), "commit");
    let receipt = f.last(&f.actor).await;
    assert_eq!(receipt.disposition, Disposition::Committed);
    assert_eq!(reply["receipt_id"], receipt.id);
    assert_eq!(receipt.credential["ref"], "author-ref");
    // Two reads under the caller's own credential, the repository and its open pull requests
    // with this branch as their head, and no platform write.
    let calls = delete_calls(&f);
    let token = blake3::hash(SECRET.as_bytes()).to_hex().to_string();
    assert_eq!(calls.len(), 2, "{calls:?}");
    for (call, path) in calls.iter().zip([
        format!("repos/{SLUG}"),
        format!("repos/{SLUG}/pulls?state=open&head=fixture:work&per_page=1"),
    ]) {
        assert_eq!(call["method"], "GET", "{call}");
        assert_eq!(call["path"], path);
        assert_eq!(call["token_hash"], token);
    }
    assert_eq!(f.remote.writes(), 0);
    // Nothing on the remote is deleted.
    assert_eq!(remote_head(&f.remote.bare), Some(f.base.clone()));
}

#[tokio::test]
async fn remote_branch_delete_refuses_a_branch_a_worktree_has_checked_out() {
    let f = Fixture::new(true, None).await;
    f.policy(&f.actor, "git.branch_delete", "allow").await;

    // The main worktree has it checked out.
    f.refusal(
        &f.actor,
        "git.branch_delete",
        delete_params(&f),
        "branch_checked_out",
    )
    .await;
    assert!(delete_calls(&f).is_empty());

    // A linked worktree holds it the same way.
    delete_detach(&f);
    let linked = f.dir.path().join("linked");
    git(
        &f.repo,
        &["worktree", "add", "-q", linked.to_str().unwrap(), "work"],
    );
    f.refusal(
        &f.actor,
        "git.branch_delete",
        delete_params(&f),
        "branch_checked_out",
    )
    .await;
    assert!(delete_calls(&f).is_empty());
    assert_eq!(work_tip(&f), Some(f.head.clone()));

    // Control: once no worktree has it, the same call deletes it.
    git(&linked, &["checkout", "-q", "--detach"]);
    f.call(&f.actor, "git.branch_delete", delete_params(&f))
        .await
        .unwrap();
    assert_eq!(work_tip(&f), None);
}

#[tokio::test]
async fn remote_branch_delete_refuses_while_a_pull_request_from_the_branch_is_open() {
    let f = Fixture::new(true, None).await;
    let n = f.open().await;
    delete_ready(&f).await;

    let error = f
        .call(&f.actor, "git.branch_delete", delete_params(&f))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("pull_request_open"), "{error}");
    assert!(
        error.contains(&format!("pull request {n} is open with this branch as its head")),
        "{error}"
    );
    let receipt = f.last(&f.actor).await;
    assert_eq!(receipt.disposition, Disposition::NotCommitted);
    assert_eq!(receipt.reason.as_deref(), Some("pull_request_open"));
    assert_eq!(work_tip(&f), Some(f.head.clone()));

    // Control: a closed pull request does not hold the branch.
    f.remote.state.lock().unwrap().prs.get_mut(&n).unwrap()["state"] = json!("closed");
    f.call(&f.actor, "git.branch_delete", delete_params(&f))
        .await
        .unwrap();
    assert_eq!(work_tip(&f), None);
}

#[tokio::test]
async fn remote_branch_delete_refuses_when_the_platform_does_not_answer_the_question() {
    let f = Fixture::new(true, None).await;
    delete_ready(&f).await;
    type Unanswered = fn(&mut State);
    let unanswered: [(&str, Unanswered); 5] = [
        ("remote_unavailable", |s| s.api_failure = true),
        ("remote_rate_limited", |s| s.rate_limited = Some(1_700_000_000)),
        ("repository_identity_mismatch", |s| {
            s.mismatch = Some("visibility")
        }),
        ("repository_identity_mismatch", |s| s.mismatch = Some("slug")),
        // A reply that is not a list of pull requests is no answer either.
        ("remote_response", |s| {
            s.open_pulls_reply = Some(json!({"message":"Not Found"}))
        }),
    ];
    for (reason, set) in unanswered {
        {
            let mut s = f.remote.state.lock().unwrap();
            s.api_failure = false;
            s.rate_limited = None;
            s.mismatch = None;
            s.open_pulls_reply = None;
            set(&mut s);
        }
        f.refusal(&f.actor, "git.branch_delete", delete_params(&f), reason)
            .await;
        assert_eq!(work_tip(&f), Some(f.head.clone()), "{reason}");
    }

    // Control: with the platform answering again, the same call deletes the branch.
    {
        let mut s = f.remote.state.lock().unwrap();
        s.mismatch = None;
        s.open_pulls_reply = None;
    }
    f.call(&f.actor, "git.branch_delete", delete_params(&f))
        .await
        .unwrap();
    assert_eq!(work_tip(&f), None);
}

#[tokio::test]
async fn remote_branch_delete_refuses_a_tip_that_moves_after_the_checks_and_keeps_the_ref() {
    let f = Fixture::new(true, None).await;
    delete_ready(&f).await;
    let middle = f.middle.clone();
    let race = DeleteRace::set(Arc::new(move |repo: &Path, branch: &str| {
        git(
            repo,
            &["update-ref", &format!("refs/heads/{branch}"), &middle],
        );
    }));

    let error = f
        .call(&f.actor, "git.branch_delete", delete_params(&f))
        .await
        .unwrap_err()
        .to_string();
    drop(race);

    // The compare in the delete refused: nothing was deleted, and the receipt says so.
    assert!(error.contains("expected_head_mismatch"), "{error}");
    let receipt = f.last(&f.actor).await;
    assert_eq!(receipt.disposition, Disposition::NotCommitted);
    assert_eq!(work_tip(&f), Some(f.middle.clone()));
    assert_eq!(receipt.result["from"], f.head);
    assert_eq!(f.remote.writes(), 0);
}

#[tokio::test]
async fn remote_branch_delete_tells_a_branch_removed_after_the_checks_from_a_moved_one() {
    let f = Fixture::new(true, None).await;
    delete_ready(&f).await;
    let race = DeleteRace::set(Arc::new(|repo: &Path, branch: &str| {
        git(repo, &["update-ref", "-d", &format!("refs/heads/{branch}")]);
    }));

    let error = f
        .call(&f.actor, "git.branch_delete", delete_params(&f))
        .await
        .unwrap_err()
        .to_string();
    drop(race);

    assert!(error.contains("branch_absent"), "{error}");
    let receipt = f.last(&f.actor).await;
    assert_eq!(receipt.disposition, Disposition::NotCommitted);
    assert_eq!(work_tip(&f), None);
}

#[tokio::test]
async fn remote_branch_delete_refuses_a_stale_tip_and_an_absent_branch_before_the_platform() {
    let f = Fixture::new(true, None).await;
    delete_ready(&f).await;

    f.refusal(
        &f.actor,
        "git.branch_delete",
        json!({"name":"work","expected":f.middle}),
        "expected_head_mismatch",
    )
    .await;
    assert!(delete_calls(&f).is_empty());
    assert_eq!(work_tip(&f), Some(f.head.clone()));

    f.call(&f.actor, "git.branch_delete", delete_params(&f))
        .await
        .unwrap();
    let reads = delete_calls(&f).len();
    // A branch that is already gone has its own answer, apart from a tip that differs.
    f.refusal(
        &f.actor,
        "git.branch_delete",
        delete_params(&f),
        "branch_absent",
    )
    .await;
    assert_eq!(delete_calls(&f).len(), reads);

    // The zero object id is never a tip: `update-ref -d` would read it as no compare at all.
    let error = f
        .call(
            &f.actor,
            "git.branch_delete",
            json!({"name":"work","expected":"0".repeat(40)}),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("invalid_params"), "{error}");
}

#[tokio::test]
async fn remote_branch_delete_refuses_a_branch_outside_the_allowlist_row_before_any_git_call() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("git-calls");
    let wrapper = dir.path().join("git");
    let f = Fixture::with_program(true, None, &[], || {
        let found = Command::new("/usr/bin/which").arg("git").output().unwrap();
        assert!(found.status.success());
        let found = PathBuf::from(String::from_utf8(found.stdout).unwrap().trim());
        std::fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\nexec {} \"$@\"\n",
                quote(&log),
                quote(&found)
            ),
        )
        .unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
        Some(wrapper.clone())
    })
    .await;
    delete_ready(&f).await;
    git(&f.repo, &["update-ref", "refs/heads/other", &f.head]);
    let before = std::fs::read_to_string(&log).ok();

    let receipt = f
        .refusal(
            &f.actor,
            "git.branch_delete",
            json!({"name":"other","expected":f.head}),
            "branch_not_allowed",
        )
        .await;

    assert_eq!(receipt.gate["decision"], "deny");
    assert_eq!(std::fs::read_to_string(&log).ok(), before);
    assert!(delete_calls(&f).is_empty());
    assert!(!f.dir.path().join("reads").exists());
    assert_eq!(
        git(&f.repo, &["rev-parse", "refs/heads/other"]),
        f.head.clone()
    );

    // Control: the same program records the calls of a delete the row allows.
    f.call(&f.actor, "git.branch_delete", delete_params(&f))
        .await
        .unwrap();
    let calls = std::fs::read_to_string(&log).unwrap();
    assert!(calls.contains("update-ref"), "{calls}");
}

#[tokio::test]
async fn remote_branch_delete_refuses_without_an_allowing_policy() {
    let f = Fixture::new(true, None).await;
    delete_detach(&f);
    f.policy(&f.actor, "git.branch_delete", "deny").await;
    let receipt = f
        .refusal(
            &f.actor,
            "git.branch_delete",
            delete_params(&f),
            "policy_denied",
        )
        .await;
    assert_eq!(receipt.policy["decision"], "deny");
    assert!(delete_calls(&f).is_empty());
    assert!(!f.dir.path().join("reads").exists());
    assert_eq!(work_tip(&f), Some(f.head.clone()));
}

#[cfg(feature = "contract-faults")]
#[tokio::test]
async fn remote_branch_delete_lost_reply_stays_unknown_and_blocks_no_later_call() {
    let f = Fixture::new(true, Some("git.branch_delete:reply-lost-after-effect")).await;
    delete_ready(&f).await;
    f.policy(&f.actor, "git.branch", "allow").await;

    let error = f
        .call(&f.actor, "git.branch_delete", delete_params(&f))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("post_effect_fault"), "{error}");
    let receipt = f.last(&f.actor).await;
    assert_eq!(receipt.disposition, Disposition::Unknown);
    assert_eq!(work_tip(&f), None);

    // An absent branch shows the goal state, not who removed it: the receipt stays unknown.
    let reconciled = f
        .call(&f.actor, "git.reconcile", json!({"receipt":receipt.id}))
        .await
        .unwrap();
    assert_eq!(reconciled["observed"], "ref_absent");
    assert_eq!(reconciled["receipt"]["disposition"], "unknown");

    // The unknown receipt holds nothing: a second delete answers that the branch is absent,
    // and the name can be made again.
    f.refusal(
        &f.actor,
        "git.branch_delete",
        delete_params(&f),
        "branch_absent",
    )
    .await;
    f.call(
        &f.actor,
        "git.branch",
        json!({"name":"work","from":f.head,"expected":f.head}),
    )
    .await
    .unwrap();
    assert_eq!(work_tip(&f), Some(f.head.clone()));

    // A branch made again at the same commit is not evidence about this receipt either.
    let reconciled = f
        .call(&f.actor, "git.reconcile", json!({"receipt":receipt.id}))
        .await
        .unwrap();
    assert_eq!(reconciled["observed"], "ref_at_expected");
    assert_eq!(reconciled["receipt"]["disposition"], "unknown");
    let stored = receipts::load_owned(&f.rt, "local", &f.actor, &receipt.id)
        .await
        .unwrap();
    assert_eq!(stored.disposition, Disposition::Unknown);
}
