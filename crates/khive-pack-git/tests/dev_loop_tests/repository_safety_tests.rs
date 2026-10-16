#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn init_makes_an_allowlisted_empty_directory_a_repository_and_records_a_receipt() {
    if crate::test_process::run_in_child() {
        return;
    }

    let f = Fixture::new(true, true).await;
    f.policy("git.init", "allow").await;
    f.policy("git.status", "allow").await;
    assert!(
        !f.blank.join(".git").exists(),
        "precondition: the target is not a repository yet"
    );
    let before = f
        .call("git.receipts", json!({"limit":500,"offset":0}))
        .await["receipts"]
        .as_array()
        .expect("receipts array")
        .len();

    let created = f
        .call("git.init", json!({"repo": f.blank, "branch": "trunk"}))
        .await;
    assert_eq!(created["branch"], json!("trunk"), "{created}");
    assert!(created["receipt_id"].as_str().is_some(), "{created}");
    assert!(f.blank.join(".git").exists(), "the repository was created");
    assert!(
        !f.blank.join(".git/hooks/pre-commit.sample").exists(),
        "--template= must leave no sample hooks behind"
    );

    let after = f
        .call("git.receipts", json!({"limit":500,"offset":0}))
        .await["receipts"]
        .as_array()
        .expect("receipts array")
        .len();
    assert_eq!(after, before + 1, "git.init is a write and takes a receipt");

    // The new repository is readable through the same pack that made it.
    let status = f.call("git.status", json!({"repo": f.blank})).await;
    assert_eq!(status["branch"]["head"], json!("trunk"), "{status}");
    assert_eq!(
        status["branch"]["oid"],
        json!(null),
        "an unborn branch reports a null oid, not the porcelain sentinel: {status}"
    );
}

#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn init_refuses_a_target_that_already_holds_a_repository_and_leaves_it_untouched() {
    if crate::test_process::run_in_child() {
        return;
    }

    let f = Fixture::new(true, true).await;
    f.policy("git.init", "allow").await;
    let head_before = f.git_text(&["rev-parse", "HEAD"]);
    let config_before = std::fs::read(f.repo.join(".git/config")).expect("config before");

    let error = f.err("git.init", json!({"repo": f.repo})).await;
    assert!(
        error.contains("already_initialized"),
        "reinitializing a live repository must refuse: {error}"
    );
    assert_eq!(
        head_before,
        f.git_text(&["rev-parse", "HEAD"]),
        "the refused init moved HEAD"
    );
    assert_eq!(
        config_before,
        std::fs::read(f.repo.join(".git/config")).expect("config after"),
        "the refused init rewrote configuration in place, which is the reason it refuses"
    );

    // Control: the same call shape succeeds against a target that holds no repository, so the
    // refusal above is the precondition and not a broken fixture.
    f.call("git.init", json!({"repo": f.blank})).await;
}

#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn init_refuses_a_directory_that_is_not_allowlisted() {
    if crate::test_process::run_in_child() {
        return;
    }

    let f = Fixture::new(true, true).await;
    f.policy("git.init", "allow").await;
    let outside = f.dir.path().join("unlisted-target");
    std::fs::create_dir(&outside).expect("unlisted directory");
    let error = f.err("git.init", json!({"repo": outside})).await;
    assert!(
        error.contains("repo_not_allowlisted"),
        "the operator's allowlist is what decides where a repository may appear: {error}"
    );
    assert!(
        !outside.join(".git").exists(),
        "a refused init created a repository anyway"
    );
}

// -- ADR-182 Amendment 8, hardening: a read verb never runs a program the REPOSITORY names -------

/// `git.status` and `git.log` reach repository-configured programs by two different doors, and
/// both are opened by ordinary config in the repository being read: a content filter driver runs
/// on the clean path when `status` has to hash a worktree file, and a signature verifier runs on
/// `log` when `log.showSignature` is set and the commit carries a signature. Each arm here fires
/// the marker on the unhardened control first, because a marker that never fires proves nothing
/// about the hardened call.
#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn read_verbs_never_execute_repository_configured_filters_or_signers() {
    if crate::test_process::run_in_child() {
        return;
    }

    let f = Fixture::new(true, true).await;
    f.policy("git.status", "allow").await;
    f.policy("git.log", "allow").await;
    let filter_marker = f.dir.path().join("read-filter.marker");
    let signer_marker = f.dir.path().join("read-signer.marker");
    let filter = f.dir.path().join("read-filter");
    executable(
        &filter,
        &format!(
            "#!/bin/sh\nprintf filter >> {}\ncat\n",
            quoted(&filter_marker)
        ),
    );
    // Signs on request and verifies on request, so it is reached from both directions. A signer
    // stands in for gpg on both paths: on the signing path git reads `[GNUPG:] SIG_CREATED ` from
    // the status descriptor and takes the armour on stdout as the signature, and on the verifying
    // path it passes `--verify` and reads the exit status.
    let signer = f.dir.path().join("read-signer");
    executable(
        &signer,
        &format!(
            r#"#!/bin/sh
printf signer >> {}
# Drain the payload git streams in before exiting, as gpg does; a signer that exits with the
# payload unread leaves git's write racing the exit and failing with EPIPE.
cat >/dev/null
for a in "$@"; do
  if [ "$a" = --verify ]; then exit 0; fi
done
printf '\n[GNUPG:] SIG_CREATED B fixture\n' >&2
printf -- '-----BEGIN PGP SIGNATURE-----\n\nfixture\n-----END PGP SIGNATURE-----\n'
exit 0
"#,
            quoted(&signer_marker)
        ),
    );

    // A tracked file the attributes bind to the driver, then a worktree change, so status has to
    // hash the file rather than trust the stat cache.
    std::fs::write(
        f.repo.join(".gitattributes"),
        b"tracked.txt filter=hostile\n",
    )
    .expect("attributes");
    std::fs::write(f.repo.join("tracked.txt"), b"one\n").expect("tracked file");
    f.git_bytes(&["add", "tracked.txt", ".gitattributes"]);
    f.git_bytes(&["commit", "-m", "tracked"]);
    f.git_bytes(&["config", "filter.hostile.clean", filter.to_str().unwrap()]);
    f.git_bytes(&["config", "filter.hostile.smudge", filter.to_str().unwrap()]);
    f.git_bytes(&["config", "filter.hostile.required", "true"]);
    f.git_bytes(&["config", "log.showSignature", "true"]);
    f.git_bytes(&["config", "gpg.program", signer.to_str().unwrap()]);
    // The signed commit is made through UNHARDENED git: the fixture's hardened helper passes
    // commit.gpgsign=false, which is the very door this arm is about.
    output(
        command(
            &f.git,
            &f.repo,
            &["commit", "--allow-empty", "-S", "-m", "signed"],
            false,
        ),
        None,
    );
    std::fs::write(f.repo.join("tracked.txt"), b"two\n").expect("modify tracked file");
    assert!(
        signer_marker.exists(),
        "the signing side of the control never ran, so the commit carries no signature"
    );
    std::fs::remove_file(&signer_marker).expect("clear signer marker after signing");

    // Positive controls, native git with none of the pack's hardening.
    output(
        command(&f.git, &f.repo, &["status", "--porcelain=v2"], false),
        None,
    );
    assert!(
        filter_marker.exists(),
        "clean-filter control is inert: this arm would pass with the defect present"
    );
    output(
        command(
            &f.git,
            &f.repo,
            &["log", "-n", "1", "--show-signature"],
            false,
        ),
        None,
    );
    assert!(
        signer_marker.exists(),
        "signature-verifier control is inert: this arm would pass with the defect present"
    );
    std::fs::remove_file(&filter_marker).expect("clear filter marker");
    std::fs::remove_file(&signer_marker).expect("clear signer marker");

    let status = f.call("git.status", json!({"repo": f.repo})).await;
    assert!(
        !filter_marker.exists(),
        "git.status ran the repository's clean filter"
    );
    assert!(
        status_paths(&status).contains(&"tracked.txt".to_string()),
        "the neutralised filter also neutralised the answer: {status}"
    );
    let log = f.call("git.log", json!({"repo": f.repo, "limit": 1})).await;
    assert!(
        !signer_marker.exists(),
        "git.log ran the repository's signature verifier"
    );
    assert_eq!(log["commits"][0]["subject"], "signed");
}

/// `git init` creates the repository directory and then writes and reads HEAD. A failure after the
/// directory exists must not settle as `not_committed`, because the directory is there. The shim
/// makes that reachable by refusing exactly the second step, on exactly this repository: it is on
/// PATH for the whole process, so a shim that refused for everyone would break whatever else is
/// running beside this test.
#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn init_reports_an_unestablished_outcome_when_it_leaves_a_repository_behind() {
    if crate::test_process::run_in_child() {
        return;
    }

    let f = Fixture::new(true, true).await;
    f.policy("git.init", "allow").await;
    let shim_dir = f.dir.path().join(SHIM_DIR);
    std::fs::create_dir(&shim_dir).expect("shim directory");
    executable(
        &shim_dir.join("git"),
        &format!(
            "#!/bin/sh\nmine=0\nfor a in \"$@\"; do\n  if [ \"$a\" = {} ]; then mine=1; fi\n             done\nif [ \"$mine\" = 1 ]; then\n  for a in \"$@\"; do\n                 if [ \"$a\" = symbolic-ref ]; then exit 1; fi\n  done\nfi\nexec {} \"$@\"\n",
            quoted(&f.blank),
            quoted(&f.git)
        ),
    );
    let path = std::env::join_paths([
        shim_dir.clone(),
        PathBuf::from("/usr/bin"),
        PathBuf::from("/bin"),
    ])
    .expect("shim PATH");
    let _env = EnvGuard::set(&[("PATH", path)]);

    let error = f.err("git.init", json!({"repo": f.blank})).await;
    assert!(
        f.blank.join(".git").exists(),
        "the shim did not reach the state this arm is about: no repository was created"
    );
    assert!(
        error.contains("init_partial_repository"),
        "a half-made repository settled without naming the condition: {error}"
    );
    let id: String = error
        .split("receipt_id=")
        .nth(1)
        .expect("error names receipt")
        .chars()
        .take_while(|c| c.is_ascii_hexdigit() || *c == '-')
        .collect();
    let receipt = f.receipt(&id).await;
    assert_eq!(
        receipt["disposition"], "unknown",
        "a repository that exists is not a write that did not happen: {receipt}"
    );
    assert_eq!(
        receipt["repo"],
        json!(f.blank.to_str().unwrap()),
        "the receipt has to name the directory somebody now has to look at: {receipt}"
    );
}

/// The resolver's own guard, because the failure it prevents is invisible from any single test:
/// PATH is process-global, so while the shim case above holds it, a fixture built concurrently
/// resolves git through the shim and caches a path that is deleted with that case's fixture.
#[test]
#[serial_test::serial(git_dev_loop_env)]
fn the_native_git_resolver_never_answers_a_fixture_shim() {
    if crate::test_process::run_in_child() {
        return;
    }

    let native = git_program();
    let dir = tempfile::tempdir().expect("temp dir");
    let shim_dir = dir.path().join(SHIM_DIR);
    std::fs::create_dir(&shim_dir).expect("shim directory");
    executable(&shim_dir.join("git"), "#!/bin/sh\nexit 7\n");
    // Control: the shim IS what a PATH search would find first, so this arm is not vacuous.
    assert!(shim_dir.join("git").is_file());
    // The shim goes in FRONT of the real PATH rather than replacing it, so the only difference
    // between the two resolutions is the shim itself.
    let mut entries = vec![shim_dir.clone()];
    entries.extend(std::env::split_paths(
        &std::env::var_os("PATH").expect("PATH"),
    ));
    let path = std::env::join_paths(entries).expect("shim PATH");
    let _env = EnvGuard::set(&[("PATH", path)]);
    let resolved = git_program();
    assert!(
        !resolved.starts_with(dir.path()),
        "a fixture would have cached {resolved:?}, which disappears with this directory"
    );
    assert_eq!(resolved, native, "the resolver moved off the native binary");
}
