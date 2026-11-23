// git.fetch over a local mapping, through the real transport. The fetch runs in a scratch
// repository and moves exactly one ref of the clone by compare-and-swap.

impl Fixture {
    async fn allow(&self, verb: &str) {
        self.registry
            .dispatch(
                "tool.policy",
                json!({"actor":self.actor,"tool":verb,"decision":"allow"}),
            )
            .await
            .unwrap();
    }
    /// A new commit in the remote on `parent`'s tree, written to `reference` there.
    fn remote_commit(&self, reference: &str, parent: &str, message: &str) -> String {
        let tree = git(&self.bare, &["rev-parse", &format!("{parent}^{{tree}}")]);
        let commit = git(
            &self.bare,
            &["commit-tree", &tree, "-p", parent, "-m", message],
        );
        git(&self.bare, &["update-ref", reference, &commit]);
        commit
    }
    /// The value of exactly `reference` in the clone; refs below it do not answer.
    fn clone_ref(&self, reference: &str) -> Option<String> {
        git(
            &self.repo,
            &["for-each-ref", "--format=%(objectname) %(refname)", reference],
        )
        .lines()
        .find_map(|line| {
            let (sha, name) = line.split_once(' ')?;
            (name == reference).then(|| sha.to_string())
        })
    }
    fn no_scratch_refs(&self) {
        assert_eq!(git(&self.repo, &["for-each-ref", "refs/khive/"]), "");
    }
    async fn fetch(&self, params: Value) -> Result<Value, RuntimeError> {
        self.call(&self.actor, "git.fetch", params).await
    }
}

#[tokio::test]
async fn local_fetch_moves_one_remote_tracking_ref_from_a_scratch_repository() {
    let f = Fixture::file().await;
    f.allow("git.fetch").await;
    let feature = f.remote_commit("refs/heads/feature", &f.head, "feature");
    // The clone's own settings and hooks must not reach the fetch: this rewrite would send it to
    // a path that does not exist, and each hook would leave a marker.
    git(
        &f.repo,
        &[
            "config",
            "url.file:///nonexistent-fetch-fixture/.insteadOf",
            "file://",
        ],
    );
    let hook_marker = f.dir.path().join("clone-hook-ran");
    let hooks = f.repo.join(".git/hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    for name in ["reference-transaction", "post-checkout", "pre-auto-gc"] {
        let hook = hooks.join(name);
        std::fs::write(
            &hook,
            format!("#!/bin/sh\ntouch '{}'\n", hook_marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let config = std::fs::read(f.repo.join(".git/config")).unwrap();

    let reply = f.fetch(json!({"ref":"feature"})).await.unwrap();
    assert_eq!(reply["ref"], "refs/remotes/origin/feature");
    assert_eq!(reply["sha"], feature);
    assert_eq!(reply["previous_sha"], Value::Null);
    assert_eq!(reply["remote"], file_url(&f.bare));
    assert_eq!(
        f.clone_ref("refs/remotes/origin/feature").as_deref(),
        Some(feature.as_str())
    );
    assert_eq!(git(&f.repo, &["cat-file", "-t", &feature]), "commit");
    f.no_scratch_refs();
    assert!(!hook_marker.exists(), "a clone hook ran during the fetch");
    assert_eq!(std::fs::read(f.repo.join(".git/config")).unwrap(), config);
    let receipt = f.last(&f.actor).await;
    assert_eq!(receipt.disposition, Disposition::Committed);
    assert_eq!(receipt.credential, json!({"source":"none"}));
    assert_eq!(reply["receipt_id"], receipt.id);
    assert_eq!(
        git(
            &f.repo,
            &["reflog", "show", "--format=%gs", "-n", "1", "refs/remotes/origin/feature"]
        ),
        format!("khive-receipt:{}", receipt.id)
    );

    // The remote moves; the next fetch moves the ref from the value it held.
    let next = f.remote_commit("refs/heads/feature", &feature, "feature 2");
    let reply = f.fetch(json!({"ref":"feature"})).await.unwrap();
    assert_eq!(reply["previous_sha"], feature);
    assert_eq!(reply["sha"], next);
    assert_eq!(
        f.clone_ref("refs/remotes/origin/feature").as_deref(),
        Some(next.as_str())
    );

    // Nothing moved at the remote: the reply says so and the ref is untouched.
    let reply = f.fetch(json!({"ref":"feature"})).await.unwrap();
    assert_eq!(reply["previous_sha"], next);
    assert_eq!(reply["sha"], next);
    assert_eq!(f.last(&f.actor).await.disposition, Disposition::Committed);
    f.no_scratch_refs();
    assert!(!hook_marker.exists());
    f.no_credential_read();
}

#[tokio::test]
async fn local_fetch_of_a_pull_head_lands_outside_remote_tracking_refs() {
    let f = Fixture::file().await;
    f.allow("git.fetch").await;
    let pull = f.remote_commit("refs/pull/7/head", &f.head, "pull head");
    // A branch literally named pull/7 is not what pull/7 means.
    git(&f.bare, &["update-ref", "refs/heads/pull/7", &f.base]);
    let reply = f.fetch(json!({"ref":"pull/7"})).await.unwrap();
    assert_eq!(reply["ref"], "refs/fetched/pull/7");
    assert_eq!(reply["sha"], pull);
    assert_eq!(
        f.clone_ref("refs/fetched/pull/7").as_deref(),
        Some(pull.as_str())
    );
    assert_eq!(
        git(&f.repo, &["for-each-ref", "--points-at", &pull, "refs/remotes/"]),
        ""
    );
    // Still unpublished: no remote-tracking ref reaches the pull head.
    let unpublished = git(&f.repo, &["rev-list", &pull, "--not", "--remotes"]);
    assert_eq!(unpublished.lines().next(), Some(pull.as_str()));
    f.no_scratch_refs();

    // Control: the same commit fetched as a branch is published.
    git(&f.bare, &["update-ref", "refs/heads/pullup", &pull]);
    f.fetch(json!({"ref":"pullup"})).await.unwrap();
    assert_eq!(
        git(&f.repo, &["rev-list", &pull, "--not", "--remotes"]),
        ""
    );
    f.no_credential_read();
}

#[tokio::test]
async fn local_fetch_refusals_leave_every_clone_ref_unchanged() {
    let f = Fixture::file().await;
    f.allow("git.fetch").await;
    let feature = f.remote_commit("refs/heads/feature", &f.head, "feature");
    let receipt = f
        .refusal(
            "git.fetch",
            json!({"ref":"feature","expected_remote":f.base}),
            "expected_remote_mismatch",
        )
        .await;
    assert_eq!(receipt.result["sha"], feature);
    assert_eq!(receipt.credential, json!({"source":"none"}));
    f.refusal("git.fetch", json!({"ref":"missing"}), "remote_ref_absent")
        .await;
    for params in [
        json!({"ref":"feature","expected_remote":null}),
        json!({"ref":"feature","expected_remote":"abc"}),
        json!({"ref":"pull/07"}),
        json!({"ref":"pull/0"}),
        json!({"ref":"pull/"}),
        json!({"ref":"-feature"}),
        json!({"ref":"feature..x"}),
    ] {
        let before = git(&f.repo, &["show-ref"]);
        assert!(f.fetch(params.clone()).await.is_err(), "{params}");
        assert_eq!(git(&f.repo, &["show-ref"]), before, "{params}");
    }
    f.no_scratch_refs();
    // Control: the right expectation moves the ref.
    f.fetch(json!({"ref":"feature","expected_remote":feature}))
        .await
        .unwrap();
    assert_eq!(
        f.clone_ref("refs/remotes/origin/feature").as_deref(),
        Some(feature.as_str())
    );
    f.no_credential_read();
}

#[tokio::test]
async fn local_fetch_refuses_objects_that_fail_the_object_check() {
    let f = Fixture::file().await;
    f.allow("git.fetch").await;
    let tree = git(&f.bare, &["rev-parse", &format!("{}^{{tree}}", f.base)]);
    let object = f.dir.path().join("bad-commit");
    std::fs::write(
        &object,
        format!(
            "tree {tree}\nparent {}\nauthor Fixture <fixture@example.invalid> notadate\ncommitter Fixture <fixture@example.invalid> 0 +0000\n\nbad\n",
            f.base
        ),
    )
    .unwrap();
    let bad = git(
        &f.bare,
        &[
            "hash-object",
            "-t",
            "commit",
            "--literally",
            "-w",
            object.to_str().unwrap(),
        ],
    );
    git(&f.bare, &["update-ref", "refs/heads/bad", &bad]);
    f.refusal("git.fetch", json!({"ref":"bad"}), "objects_refused")
        .await;
    assert_eq!(f.clone_ref("refs/remotes/origin/bad"), None);
    f.no_scratch_refs();
}

/// A git program that runs `race.sh` once, just before the transfer, and is the real git otherwise.
fn racing_program(dir: &Path) -> (PathBuf, PathBuf) {
    let native = Command::new("/usr/bin/which").arg("git").output().unwrap();
    assert!(native.status.success());
    let native = String::from_utf8(native.stdout).unwrap();
    let quote = |value: &str| format!("'{}'", value.replace('\'', "'\\''"));
    let race = dir.join("race.sh");
    let program = dir.join("racing-git");
    std::fs::write(
        &program,
        format!(
            "#!/bin/sh\ncase \" $* \" in *\" fetch --quiet \"*) if [ -f {race} ]; then . {race}; /bin/rm -f {race}; fi;; esac\nexec {native} \"$@\"\n",
            race = quote(race.to_str().unwrap()),
            native = quote(native.trim())
        ),
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    (program, race)
}

/// A refusal raised after the race changed a clone ref, so the clone's refs are not compared.
async fn raced(f: &Fixture, reason: &str) {
    let error = f
        .fetch(json!({"ref":"feature"}))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains(reason), "{error}");
    let receipt = f.last(&f.actor).await;
    assert!(error.contains(&receipt.id));
    assert_eq!(receipt.reason.as_deref(), Some(reason));
    assert_eq!(receipt.disposition, Disposition::NotCommitted);
}

#[tokio::test]
async fn local_fetch_compare_and_swap_refuses_a_ref_that_moved_or_is_locked() {
    let dir = tempfile::tempdir().unwrap();
    let (program, race) = racing_program(dir.path());
    let f = Fixture::with_program(file_url, "", || Some(program.clone())).await;
    f.allow("git.fetch").await;
    let feature = f.remote_commit("refs/heads/feature", &f.head, "feature");
    let destination = f.repo.join(".git/refs/remotes/origin");
    let quote = |path: &Path| format!("'{}'", path.display().to_string().replace('\'', "'\\''"));

    // Another writer creates the ref after it was read as absent.
    std::fs::write(
        &race,
        format!(
            "/bin/mkdir -p {dir}\nprintf '%s\\n' {base} > {file}\n",
            dir = quote(&destination),
            base = f.base,
            file = quote(&destination.join("feature"))
        ),
    )
    .unwrap();
    raced(&f, "ref_moved").await;
    assert!(!race.exists());
    assert_eq!(
        f.clone_ref("refs/remotes/origin/feature").as_deref(),
        Some(f.base.as_str())
    );
    // A retry reads the new value and moves from it.
    let reply = f.fetch(json!({"ref":"feature"})).await.unwrap();
    assert_eq!(reply["previous_sha"], f.base);
    assert_eq!(reply["sha"], feature);

    // Another writer holds the ref's lock.
    let next = f.remote_commit("refs/heads/feature", &feature, "feature 2");
    let lock = destination.join("feature.lock");
    std::fs::write(&race, format!(": > {}\n", quote(&lock))).unwrap();
    raced(&f, "ref_locked").await;
    assert!(!race.exists());
    assert_eq!(
        f.clone_ref("refs/remotes/origin/feature").as_deref(),
        Some(feature.as_str())
    );
    std::fs::remove_file(&lock).unwrap();
    let reply = f.fetch(json!({"ref":"feature"})).await.unwrap();
    assert_eq!(reply["previous_sha"], feature);
    assert_eq!(reply["sha"], next);
    f.no_scratch_refs();
    f.no_credential_read();
}
